//! Native backend of emacs-egui: a dynamic module that renders egui with Metal
//! into an `NSView` it creates inside the Emacs process.
//!
//! The view is placed by Emacs, not by this module: `emacs-egui-native-make-view`
//! returns a `user-ptr` whose pointer is the view, ready for
//! `(make-xwidget 'native TITLE WIDTH HEIGHT VIEW)`, and Emacs's redisplay moves,
//! clips, resizes and hides it as it does a WebKit xwidget. That needs the
//! `native` xwidget type, an Emacs patch (docs/design/100-embedded-widgets.md, N1).
//!
//! Threads:
//! - Every `emacs-egui-native-*` function runs on the Emacs main thread, which
//!   is also the AppKit main thread. All AppKit objects are touched there.
//! - Each view has a render thread of its own. It owns the egui context and the
//!   wgpu surface, and draws into a `CAMetalLayer` without the main thread, so
//!   it keeps drawing while Elisp is busy and Emacs is not pumping the run loop.
//!   It draws only when there is something to draw: input, a push, a resize, an
//!   unhide, or a repaint egui asked for. Otherwise it sleeps, and while the view
//!   is hidden it sleeps whatever egui asked.
//! - One writer thread drains events into the pipe process that Elisp opened
//!   with `emacs-egui-native-open-channel`, so a full pipe never stalls a frame.

use std::{
    cell::RefCell,
    ffi::c_void,
    io::Write,
    panic::{self, AssertUnwindSafe},
    ptr::{self, NonNull},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use egui_wgpu::wgpu;
use emacs::{
    defun,
    raw::{emacs_env, emacs_value},
    Env, IntoLisp, Result, Value,
};
use objc2::{
    define_class, msg_send,
    rc::{Retained, Weak},
    runtime::{AnyClass, AnyObject, ProtocolObject},
    AllocAnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSResponder, NSScreen,
    NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
};
use objc2_foundation::{
    NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
};
use objc2_quartz_core::{CALayer, CALayerDelegate, CAMetalLayer, CATransaction};

emacs::plugin_is_GPL_compatible!();

#[emacs::module(
    name = "emacs-egui-native",
    defun_prefix = "emacs-egui-native",
    separator = "-"
)]
fn init(env: &Env) -> Result<()> {
    install_panic_hook();
    // `emacs-egui-native-release' must clear its handle's pointer, and the
    // `emacs' crate has no `set_user_ptr', so that one step is a raw function.
    unsafe {
        let raw = env.raw();
        let make_function = (*raw).make_function.expect("emacs_env.make_function");
        let f = make_function(
            raw,
            1,
            1,
            Some(clear_handle),
            c"Make the view handle HANDLE point nowhere (internal).".as_ptr(),
            ptr::null_mut(),
        );
        env.call(
            "fset",
            (
                env.intern("emacs-egui-native--clear-handle")?,
                Value::new(f, env),
            ),
        )?;
    }
    Ok(())
}

/// Lock M even if a thread panicked while holding it: every value behind these
/// locks stays valid between statements, and an AppKit callback must not panic.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Panics
// ---------------------------------------------------------------------------

thread_local! {
    /// The message and location of this thread's last panic, from the hook.
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic with a non-string payload".to_string()
    }
}

/// Record each panic's message and location for `guard` and the render
/// threads, then run the previous hook.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let at = info
                .location()
                .map(|l| format!(" at {}:{}", l.file(), l.line()))
                .unwrap_or_default();
            let message = format!("{}{at}", payload_message(info.payload()));
            LAST_PANIC.with(|p| *p.borrow_mut() = Some(message));
            previous(info);
        }));
    });
}

fn take_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    LAST_PANIC
        .with(|p| p.borrow_mut().take())
        .unwrap_or_else(|| payload_message(payload))
}

/// Run a Lisp function's body. A panic in it goes on to the `emacs` crate's
/// boundary, which signals `rust-panic`, but re-raised with a `String`
/// payload: the crate reads the message only from a `String`, and a literal
/// `panic!` or an `unwrap` on `None` carries a `&str`, which would reach Lisp
/// as "Any { .. }".
fn guard<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => {
            let message = take_panic_message(&*payload);
            panic::resume_unwind(Box::new(format!(
                "Rust panic in emacs-egui-native: {message}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Events to Elisp
// ---------------------------------------------------------------------------

static CHANNEL: Mutex<Option<mpsc::Sender<String>>> = Mutex::new(None);

/// Queue one event line `(ID ACTION . PAYLOAD)` for the pipe process.
/// PAYLOAD is already Lisp syntax (a plist body, possibly empty).
fn emit(id: u64, action: &str, payload: &str) {
    let line = if payload.is_empty() {
        format!("({id} {action})\n")
    } else {
        format!("({id} {action} {payload})\n")
    };
    if let Some(tx) = lock(&CHANNEL).as_ref() {
        let _ = tx.send(line);
    }
}

/// A Lisp string literal for `s`, readable by `read`.
fn lisp_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// GPU, shared by every view
// ---------------------------------------------------------------------------

struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Held across `Surface::configure` and across `submit` and `present`.
    /// Configuring waits for the device to go idle and fails if another
    /// thread submits meanwhile, and every view's render thread shares this
    /// device.
    submit_lock: Mutex<()>,
}

static GPU: OnceLock<Arc<Gpu>> = OnceLock::new();

fn gpu() -> Result<Arc<Gpu>> {
    if let Some(g) = GPU.get() {
        return Ok(g.clone());
    }
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::METAL;
    let instance = wgpu::Instance::new(desc);
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|e| anyhow::anyhow!("no Metal adapter: {e}"))?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| anyhow::anyhow!("no Metal device: {e}"))?;
    let _ = GPU.set(Arc::new(Gpu {
        instance,
        adapter,
        device,
        queue,
        submit_lock: Mutex::new(()),
    }));
    Ok(GPU.get().expect("set above").clone())
}

// ---------------------------------------------------------------------------
// State shared between the main thread and a view's render thread
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct Geometry {
    width: f32,
    height: f32,
    scale: f32,
}

#[derive(Clone, Copy, Default)]
struct Rects {
    button: Option<egui::Rect>,
    text: Option<egui::Rect>,
}

/// Why the render thread should draw next.
#[derive(Default)]
struct Wake {
    /// Something changed: input, a push, a resize, focus, an unhide.
    dirty: bool,
    /// When egui asked to be drawn again, if it did.
    repaint_at: Option<Instant>,
}

struct Shared {
    id: u64,
    running: AtomicBool,
    focused: AtomicBool,
    /// In a window and neither hidden nor under a hidden ancestor.
    visible: AtomicBool,
    /// The pushed state asks for the animation (`:animate` non-nil).
    animate: AtomicBool,
    wake: Mutex<Wake>,
    wake_cv: Condvar,
    frames: AtomicU64,
    clicks: AtomicU64,
    hides: AtomicU64,
    unhides: AtomicU64,
    input: Mutex<Vec<egui::Event>>,
    /// The last pushed plist, as (key, printed value) pairs.
    state: Mutex<Vec<(String, String)>>,
    text: Mutex<String>,
    rects: Mutex<Rects>,
    geometry: Mutex<Geometry>,
    /// The screen rectangle egui laid out the last frame in, in points.
    egui_screen: Mutex<(f32, f32)>,
    created_at: Instant,
    first_frame_ms: Mutex<Option<f64>>,
    fps: Mutex<f64>,
    surface_format: Mutex<String>,
    /// Whether egui's text field held keyboard focus in the last frame.
    text_focused: AtomicBool,
    /// When the view was hidden, until the render thread has paused.
    hidden_at: Mutex<Option<Instant>>,
    /// When and by which callback the view became visible, until the render
    /// thread has drawn after it.
    shown_at: Mutex<Option<(Instant, &'static str)>>,
    pause_ms: Mutex<Option<f64>>,
    resume_ms: Mutex<Option<f64>>,
}

impl Shared {
    fn new(id: u64, width: f64, height: f64, scale: f64, created_at: Instant) -> Self {
        Self {
            id,
            running: AtomicBool::new(true),
            focused: AtomicBool::new(false),
            visible: AtomicBool::new(false),
            animate: AtomicBool::new(false),
            wake: Mutex::new(Wake {
                dirty: true,
                repaint_at: None,
            }),
            wake_cv: Condvar::new(),
            frames: AtomicU64::new(0),
            clicks: AtomicU64::new(0),
            hides: AtomicU64::new(0),
            unhides: AtomicU64::new(0),
            input: Mutex::new(Vec::new()),
            state: Mutex::new(Vec::new()),
            text: Mutex::new(String::new()),
            rects: Mutex::new(Rects::default()),
            geometry: Mutex::new(Geometry {
                width: width as f32,
                height: height as f32,
                scale: scale as f32,
            }),
            egui_screen: Mutex::new((0.0, 0.0)),
            created_at,
            first_frame_ms: Mutex::new(None),
            fps: Mutex::new(0.0),
            surface_format: Mutex::new(String::new()),
            text_focused: AtomicBool::new(false),
            hidden_at: Mutex::new(None),
            shown_at: Mutex::new(None),
            pause_ms: Mutex::new(None),
            resume_ms: Mutex::new(None),
        }
    }

    /// Ask the render thread for a frame.
    fn request_frame(&self) {
        lock(&self.wake).dirty = true;
        self.wake_cv.notify_all();
    }

    fn push_input(&self, event: egui::Event) {
        lock(&self.input).push(event);
        self.request_frame();
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        let _wake = lock(&self.wake);
        self.wake_cv.notify_all();
    }

    fn label(&self) -> String {
        lock(&self.state)
            .iter()
            .find(|(k, _)| k == ":label")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "(no :label pushed)".to_string())
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Views not yet deallocated, and render threads not yet ended, for tests.
static LIVE_VIEWS: AtomicI64 = AtomicI64::new(0);
static LIVE_THREADS: AtomicI64 = AtomicI64::new(0);

// ---------------------------------------------------------------------------
// Objective-C classes
// ---------------------------------------------------------------------------

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "EmacsEguiNativeLayerDelegate"]
    struct LayerDelegate;

    impl LayerDelegate {
        /// wgpu-hal walks up from the surface's layer to the first layer that
        /// has a delegate and asks it for its `window`, on every acquire, to skip
        /// occluded windows. That would be an AppKit call on the render thread;
        /// answering nil here keeps the render thread off AppKit entirely.
        #[unsafe(method(window))]
        fn window(&self) -> *mut AnyObject {
            ptr::null_mut()
        }
    }

    unsafe impl NSObjectProtocol for LayerDelegate {}
    unsafe impl CALayerDelegate for LayerDelegate {}
);

impl LayerDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// What a view owns while it renders; taken and dropped by `stop_render`.
struct Render {
    layer: Retained<CAMetalLayer>,
    _delegate: Retained<LayerDelegate>,
    thread: JoinHandle<()>,
}

struct ViewIvars {
    shared: Arc<Shared>,
    render: RefCell<Option<Render>>,
}

impl Drop for ViewIvars {
    fn drop(&mut self) {
        LIVE_VIEWS.fetch_sub(1, Ordering::SeqCst);
    }
}

define_class!(
    // SAFETY: NSView has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EmacsEguiNativeView"]
    #[ivars = ViewIvars]
    struct EguiView;

    impl EguiView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// A click makes the view first responder; unfocused, it hands every
        /// key on to Emacs (`key_down`).
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        /// Mouse events reach egui even when the window is not key.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// To egui, and to the `EmacsView` too, as nsxwidget does, so a click
        /// selects the Emacs window the widget is in. Then the view takes the
        /// keys, which it passes on to Emacs unless focused.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.pointer_button(event, egui::PointerButton::Primary, true);
            if let Some(emacs) = self.emacs_view() {
                unsafe {
                    let _: () = msg_send![&*emacs, mouseDown: event];
                }
            }
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.pointer_button(event, egui::PointerButton::Primary, false);
            if let Some(emacs) = self.emacs_view() {
                unsafe {
                    let _: () = msg_send![&*emacs, mouseUp: event];
                }
            }
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.pointer_button(event, egui::PointerButton::Secondary, true);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.pointer_button(event, egui::PointerButton::Secondary, false);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.pointer_moved(event);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.pointer_moved(event);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().shared.push_input(egui::Event::PointerGone);
        }

        /// The wheel scrolls the buffer, not the widget (contract, *Keyboard
        /// and pointer*).
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            if let Some(emacs) = self.emacs_view() {
                unsafe {
                    let _: () = msg_send![&*emacs, scrollWheel: event];
                }
            }
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.key(event, true);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.key(event, false);
        }

        /// AppKit offers a key with a modifier to the main menu before the
        /// first responder, and Emacs's menu passes it straight to the
        /// `EmacsView`, so `C-g` and every Control key would bypass a focused
        /// view. AppKit asks the window's views first; a focused view that is
        /// first responder takes the key here.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            self.key_equivalent(event)
        }

        /// As in nsxwidget: do nothing, so the input manager never turns a key
        /// into text behind the view's back.
        #[unsafe(method(interpretKeyEvents:))]
        fn interpret_key_events(&self, _events: &AnyObject) {}

        #[unsafe(method(viewDidHide))]
        fn view_did_hide(&self) {
            self.update_visibility("viewDidHide");
        }

        #[unsafe(method(viewDidUnhide))]
        fn view_did_unhide(&self) {
            self.update_visibility("viewDidUnhide");
        }

        /// Before the view leaves its window, give the keys back to Emacs: a
        /// window whose first responder is removed makes itself first
        /// responder, and the Emacs frame would then take no key until clicked.
        #[unsafe(method(viewWillMoveToWindow:))]
        fn view_will_move_to_window(&self, new_window: Option<&NSWindow>) {
            let Some(window) = self.window() else {
                return;
            };
            if new_window.is_some_and(|w| ptr::eq(w, &*window)) {
                return;
            }
            if self.is_first_responder(&window) {
                let emacs = self.emacs_view();
                window.makeFirstResponder(emacs.as_deref().map(|v| -> &NSResponder { v }));
            }
        }

        #[unsafe(method(viewDidMoveToWindow))]
        fn view_did_move_to_window(&self) {
            if let Some(window) = self.window() {
                let scale = window.backingScaleFactor();
                lock(&self.ivars().shared.geometry).scale = scale as f32;
                if let Some(render) = self.ivars().render.borrow().as_ref() {
                    render.layer.setContentsScale(scale);
                }
            }
            self.update_visibility("viewDidMoveToWindow");
        }

        /// Emacs sizes the view (`nsxwidget_resize`); the Metal layer and
        /// egui's screen follow.
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            unsafe {
                let _: () = msg_send![super(self), setFrameSize: size];
            }
            self.resized(size);
        }
    }

    unsafe impl NSObjectProtocol for EguiView {}
);

impl EguiView {
    fn new(mtm: MainThreadMarker, frame: NSRect, shared: Arc<Shared>) -> Retained<Self> {
        LIVE_VIEWS.fetch_add(1, Ordering::SeqCst);
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            shared,
            render: RefCell::new(None),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// The `EmacsView` of the frame the view is shown in: the first ancestor of
    /// that class (nsxwidget puts the view in a container in a container in it).
    fn emacs_view(&self) -> Option<Retained<NSView>> {
        let class = AnyClass::get(c"EmacsView")?;
        // SAFETY: on the main thread, between AppKit calls, a superview is
        // alive; the walk retains each one it returns.
        let mut next = unsafe { self.superview() };
        while let Some(view) = next {
            if view.isKindOfClass(class) {
                return Some(view);
            }
            next = unsafe { view.superview() };
        }
        None
    }

    fn is_first_responder(&self, window: &NSWindow) -> bool {
        let me: &NSResponder = self;
        window
            .firstResponder()
            .is_some_and(|first| ptr::eq(&*first, me))
    }

    fn local_pos(&self, event: &NSEvent) -> egui::Pos2 {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        egui::pos2(p.x as f32, p.y as f32)
    }

    fn pointer_button(&self, event: &NSEvent, button: egui::PointerButton, pressed: bool) {
        let pos = self.local_pos(event);
        let shared = &self.ivars().shared;
        shared.push_input(egui::Event::PointerMoved(pos));
        shared.push_input(egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: modifiers(event.modifierFlags()),
        });
    }

    fn pointer_moved(&self, event: &NSEvent) {
        let pos = self.local_pos(event);
        self.ivars()
            .shared
            .push_input(egui::Event::PointerMoved(pos));
    }

    /// The keyboard policy. Unfocused: every key goes to Emacs. Focused: keys
    /// go to egui, except `C-g`, which unfocuses and goes to Emacs.
    ///
    /// The key is a real `keyDown:`, so Emacs is inside its event loop and
    /// `EmacsView keyDown:` takes it as it is.
    fn key(&self, event: &NSEvent, down: bool) {
        let shared = self.ivars().shared.clone();
        let focused = shared.focused.load(Ordering::SeqCst);
        let quit = is_quit(event);
        if focused && !quit {
            for e in egui_key_events(event, down) {
                shared.push_input(e);
            }
            return;
        }
        if !down {
            return;
        }
        if focused {
            self.set_focused(false);
            emit(shared.id, "unfocus", ":reason quit");
        }
        if let Some(emacs) = self.emacs_view() {
            unsafe {
                let _: () = msg_send![&*emacs, keyDown: event];
            }
        }
    }

    fn key_equivalent(&self, event: &NSEvent) -> bool {
        if event.r#type() != NSEventType::KeyDown
            || !self.ivars().shared.focused.load(Ordering::SeqCst)
        {
            return false;
        }
        match self.window() {
            Some(window) if self.is_first_responder(&window) => {
                self.key(event, true);
                true
            }
            _ => false,
        }
    }

    fn set_focused(&self, focused: bool) {
        let shared = &self.ivars().shared;
        if focused {
            // One focused view at a time.
            if let Some(previous) = focused_view() {
                if !ptr::eq(&*previous, self) {
                    previous.set_focused(false);
                }
            }
            FOCUSED.with(|f| *f.borrow_mut() = Some(Weak::from_retained(&self.retain())));
            install_quit_monitor();
        } else if focused_view().is_some_and(|v| ptr::eq(&*v, self)) {
            FOCUSED.with(|f| *f.borrow_mut() = None);
        }
        shared.focused.store(focused, Ordering::SeqCst);
        shared.request_frame();
        let Some(window) = self.window() else {
            return;
        };
        if focused {
            window.makeFirstResponder(Some(self));
        } else if self.is_first_responder(&window) {
            let emacs = self.emacs_view();
            window.makeFirstResponder(emacs.as_deref().map(|v| -> &NSResponder { v }));
        }
    }

    /// Pause or resume the render thread when AppKit says the view was hidden,
    /// shown, or moved to or out of a window. Hidden includes a hidden
    /// ancestor, which is how Emacs hides a native xwidget's view.
    fn update_visibility(&self, by: &'static str) {
        let shared = &self.ivars().shared;
        let visible = self.window().is_some() && !self.isHiddenOrHasHiddenAncestor();
        if shared.visible.swap(visible, Ordering::SeqCst) == visible {
            return;
        }
        let now = Instant::now();
        if visible {
            shared.unhides.fetch_add(1, Ordering::SeqCst);
            *lock(&shared.hidden_at) = None;
            *lock(&shared.shown_at) = Some((now, by));
            emit(shared.id, "shown", &format!(":by {}", lisp_string(by)));
            shared.request_frame();
        } else {
            shared.hides.fetch_add(1, Ordering::SeqCst);
            *lock(&shared.shown_at) = None;
            *lock(&shared.hidden_at) = Some(now);
            emit(shared.id, "hidden", &format!(":by {}", lisp_string(by)));
            // Wake the render thread so it notes the pause.
            let _wake = lock(&shared.wake);
            shared.wake_cv.notify_all();
        }
    }

    fn resized(&self, size: NSSize) {
        let shared = &self.ivars().shared;
        {
            let mut geo = lock(&shared.geometry);
            geo.width = size.width as f32;
            geo.height = size.height as f32;
        }
        if let Some(render) = self.ivars().render.borrow().as_ref() {
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            render
                .layer
                .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
            CATransaction::commit();
        }
        shared.request_frame();
    }

    /// Stop the render thread and drop what it drew with. The view itself
    /// stays wherever Emacs put it, showing its last frame.
    fn stop_render(&self) {
        let Some(render) = self.ivars().render.borrow_mut().take() else {
            return;
        };
        let shared = &self.ivars().shared;
        shared.stop();
        let _ = render.thread.join();
        render.layer.setDelegate(None);
        self.set_focused(false);
        emit(shared.id, "released", "");
    }
}

thread_local! {
    /// The focused view, if any. Main thread only.
    static FOCUSED: RefCell<Option<Weak<EguiView>>> = const { RefCell::new(None) };
    /// The monitor that unfocuses on `C-g`, installed with the first focus.
    static QUIT_MONITOR: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
}

fn focused_view() -> Option<Retained<EguiView>> {
    FOCUSED.with(|f| f.borrow().as_ref().and_then(|w| w.load()))
}

/// `C-g` must unfocus the focused view whichever way AppKit routes it. When
/// the application is not active, AppKit hands a Control key to the main menu
/// without asking the window's views, and Emacs's menu passes it to the
/// `EmacsView`; the view's own `keyDown:` and `performKeyEquivalent:` never
/// see it. A local monitor sees every key-down as it is dequeued, before any
/// of that; it unfocuses and lets the key go on to Emacs unchanged.
fn install_quit_monitor() {
    QUIT_MONITOR.with(|m| {
        if m.borrow().is_some() {
            return;
        }
        let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: AppKit passes a live event for the call.
                let ev = unsafe { event.as_ref() };
                if ev.r#type() == NSEventType::KeyDown && is_quit(ev) {
                    if let Some(view) = focused_view() {
                        view.set_focused(false);
                        emit(view.ivars().shared.id, "unfocus", ":reason quit");
                    }
                }
            }));
            event.as_ptr()
        });
        let monitor = unsafe {
            NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block)
        };
        *m.borrow_mut() = monitor;
    });
}

fn is_quit(event: &NSEvent) -> bool {
    let flags = event.modifierFlags();
    let unmodified = event
        .charactersIgnoringModifiers()
        .map(|s| s.to_string())
        .unwrap_or_default();
    flags.contains(NSEventModifierFlags::Control)
        && !flags.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Option)
        && unmodified.eq_ignore_ascii_case("g")
}

fn modifiers(flags: NSEventModifierFlags) -> egui::Modifiers {
    let cmd = flags.contains(NSEventModifierFlags::Command);
    egui::Modifiers {
        alt: flags.contains(NSEventModifierFlags::Option),
        ctrl: flags.contains(NSEventModifierFlags::Control),
        shift: flags.contains(NSEventModifierFlags::Shift),
        mac_cmd: cmd,
        command: cmd,
    }
}

fn egui_key_events(event: &NSEvent, down: bool) -> Vec<egui::Event> {
    let mut out = Vec::new();
    let flags = event.modifierFlags();
    let mods = modifiers(flags);
    if let Some(key) = egui_key(event.keyCode()) {
        out.push(egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: down,
            repeat: down && event.isARepeat(),
            modifiers: mods,
        });
    }
    if down && !mods.ctrl && !mods.mac_cmd {
        if let Some(chars) = event.characters() {
            let text: String = chars
                .to_string()
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            if !text.is_empty() {
                out.push(egui::Event::Text(text));
            }
        }
    }
    out
}

/// macOS virtual key codes (HIToolbox `kVK_*`) for the keys a text field needs.
fn egui_key(code: u16) -> Option<egui::Key> {
    use egui::Key;
    Some(match code {
        0x24 => Key::Enter,
        0x30 => Key::Tab,
        0x33 => Key::Backspace,
        0x35 => Key::Escape,
        0x75 => Key::Delete,
        0x73 => Key::Home,
        0x77 => Key::End,
        0x7B => Key::ArrowLeft,
        0x7C => Key::ArrowRight,
        0x7D => Key::ArrowDown,
        0x7E => Key::ArrowUp,
        _ => return None,
    })
}

/// The ANSI virtual key code of a lowercase letter, for synthetic key events.
fn letter_key_code(c: char) -> u16 {
    const CODES: [(char, u16); 26] = [
        ('a', 0x00),
        ('s', 0x01),
        ('d', 0x02),
        ('f', 0x03),
        ('h', 0x04),
        ('g', 0x05),
        ('z', 0x06),
        ('x', 0x07),
        ('c', 0x08),
        ('v', 0x09),
        ('b', 0x0B),
        ('q', 0x0C),
        ('w', 0x0D),
        ('e', 0x0E),
        ('r', 0x0F),
        ('y', 0x10),
        ('t', 0x11),
        ('o', 0x1F),
        ('u', 0x20),
        ('i', 0x22),
        ('p', 0x23),
        ('l', 0x25),
        ('j', 0x26),
        ('k', 0x28),
        ('n', 0x2D),
        ('m', 0x2E),
    ];
    let c = c.to_ascii_lowercase();
    CODES
        .iter()
        .find(|(l, _)| *l == c)
        .map(|(_, k)| *k)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Render thread
// ---------------------------------------------------------------------------

/// The Metal layer crosses to the render thread only to re-enable the drawable
/// timeout after wgpu configures the surface; CALayer properties are safe to
/// set off the main thread.
struct SendLayer(Retained<CAMetalLayer>);
unsafe impl Send for SendLayer {}

struct UiState {
    clicks: u64,
    text: String,
    last_text: String,
    start: Instant,
}

fn draw(ui: &mut egui::Ui, st: &mut UiState, shared: &Shared, frame: u64) {
    let bg = egui::Color32::from_rgb(0x28, 0x28, 0x28);
    ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
    let animate = shared.animate.load(Ordering::SeqCst);
    egui::Frame::NONE
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.style_mut().visuals = egui::Visuals::dark();
            ui.label(egui::RichText::new(format!("frame {frame}")).monospace());
            ui.label(format!("state: {}", shared.label()));
            let mut rects = Rects::default();
            ui.horizontal(|ui| {
                let button = ui.button(format!("click me ({})", st.clicks));
                rects.button = Some(button.rect);
                if button.clicked() {
                    st.clicks += 1;
                    shared.clicks.store(st.clicks, Ordering::SeqCst);
                    emit(shared.id, "click", &format!(":count {}", st.clicks));
                }
                let focused = shared.focused.load(Ordering::SeqCst);
                ui.label(if focused { "keys: egui" } else { "keys: Emacs" });
            });
            let edit = ui.add(
                egui::TextEdit::singleline(&mut st.text)
                    .hint_text("focus the view, then type")
                    .desired_width(f32::INFINITY),
            );
            rects.text = Some(edit.rect);
            let want_focus = shared.focused.load(Ordering::SeqCst);
            shared
                .text_focused
                .store(edit.has_focus(), Ordering::SeqCst);
            if want_focus && !edit.has_focus() {
                edit.request_focus();
            } else if !want_focus && edit.has_focus() {
                edit.surrender_focus();
            }
            if st.text != st.last_text {
                st.last_text = st.text.clone();
                *lock(&shared.text) = st.text.clone();
                emit(
                    shared.id,
                    "text",
                    &format!(":value {}", lisp_string(&st.text)),
                );
            }
            // A bar that crosses the track once a second while the pushed
            // state asks for the animation, and rests at the left otherwise.
            let (track, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(track, 3.0, egui::Color32::from_gray(60));
            let t = if animate {
                st.start.elapsed().as_secs_f32() % 1.0
            } else {
                0.0
            };
            let bar_w = 40.0f32.min(track.width());
            let x = track.left() + t * (track.width() - bar_w);
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, track.top()),
                    egui::vec2(bar_w, track.height()),
                ),
                3.0,
                egui::Color32::from_rgb(0x83, 0xa5, 0x98),
            );
            if animate {
                ui.ctx().request_repaint();
            }
            *lock(&shared.rects) = rects;
        });
}

/// Take pending input for one frame. At most one pointer-button transition per
/// frame, so a press and a release delivered together still reach egui in two
/// passes, as a real click does; what is kept back asks for the next frame.
///
/// Keys wait while the view is focused but egui has not yet given the text
/// field focus: egui grants a `request_focus` one frame later, and a key in
/// between would be dropped.
fn take_input(shared: &Shared) -> Vec<egui::Event> {
    let hold_keys =
        shared.focused.load(Ordering::SeqCst) && !shared.text_focused.load(Ordering::SeqCst);
    let mut queue = lock(&shared.input);
    let mut taken = Vec::new();
    let mut kept = Vec::new();
    let mut button_taken = false;
    for e in queue.drain(..) {
        let is_key = matches!(e, egui::Event::Key { .. } | egui::Event::Text(_));
        let is_button = matches!(e, egui::Event::PointerButton { .. });
        if button_taken || (is_key && hold_keys) {
            kept.push(e);
        } else {
            button_taken |= is_button;
            taken.push(e);
        }
    }
    let more = !kept.is_empty();
    *queue = kept;
    drop(queue);
    if more {
        lock(&shared.wake).dirty = true;
    }
    taken
}

/// Block until the view has a frame to draw; false once the view stops.
/// A hidden view waits for whatever egui asked, and notes when it paused.
fn wait_for_frame(shared: &Shared) -> bool {
    let mut wake = lock(&shared.wake);
    loop {
        if !shared.running.load(Ordering::SeqCst) {
            return false;
        }
        if !shared.visible.load(Ordering::SeqCst) {
            if let Some(at) = lock(&shared.hidden_at).take() {
                let ms = at.elapsed().as_secs_f64() * 1000.0;
                *lock(&shared.pause_ms) = Some(ms);
                emit(shared.id, "paused", &format!(":ms {ms:.3}"));
            }
            wake = shared.wake_cv.wait(wake).unwrap_or_else(|e| e.into_inner());
            continue;
        }
        if wake.dirty {
            break;
        }
        match wake.repaint_at {
            Some(at) => {
                let now = Instant::now();
                if at <= now {
                    break;
                }
                wake = shared
                    .wake_cv
                    .wait_timeout(wake, at - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            None => {
                wake = shared.wake_cv.wait(wake).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
    wake.dirty = false;
    wake.repaint_at = None;
    true
}

fn render_loop(
    shared: Arc<Shared>,
    gpu: Arc<Gpu>,
    surface: wgpu::Surface<'static>,
    layer: SendLayer,
) {
    let ctx = egui::Context::default();
    let format = {
        let caps = surface.get_capabilities(&gpu.adapter);
        caps.formats
            .into_iter()
            .find(|f| !f.is_srgb())
            .unwrap_or(wgpu::TextureFormat::Bgra8Unorm)
    };
    *lock(&shared.surface_format) = format!("{format:?}");
    let mut renderer =
        egui_wgpu::Renderer::new(&gpu.device, format, egui_wgpu::RendererOptions::default());
    let mut configured: Option<(u32, u32)> = None;
    let mut st = UiState {
        clicks: 0,
        text: String::new(),
        last_text: String::new(),
        start: Instant::now(),
    };
    let mut report_at = Instant::now();
    let mut report_frames = 0u64;

    while wait_for_frame(&shared) {
        let geo = *lock(&shared.geometry);
        let px = (
            (geo.width * geo.scale).round().max(1.0) as u32,
            (geo.height * geo.scale).round().max(1.0) as u32,
        );
        if configured != Some(px) {
            let _submitting = lock(&gpu.submit_lock);
            surface.configure(
                &gpu.device,
                &wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format,
                    width: px.0,
                    height: px.1,
                    present_mode: wgpu::PresentMode::Fifo,
                    alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                    view_formats: vec![format],
                    desired_maximum_frame_latency: 2,
                    ..surface_config_defaults(&surface, &gpu, px)
                },
            );
            // wgpu turns the drawable timeout off; a hidden layer would then
            // block `nextDrawable` forever and `release` could not join us.
            layer.0.setAllowsNextDrawableTimeout(true);
            configured = Some(px);
        }

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                configured = None;
                shared.request_frame();
                continue;
            }
            _ => {
                thread::sleep(Duration::from_millis(16));
                shared.request_frame();
                continue;
            }
        };

        let frame_no = shared.frames.load(Ordering::SeqCst) + 1;
        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(geo.width, geo.height),
            )),
            time: Some(st.start.elapsed().as_secs_f64()),
            focused: true,
            events: take_input(&shared),
            ..Default::default()
        };
        raw.viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(geo.scale);
        let output = ctx.run_ui(raw, |ui| draw(ui, &mut st, &shared, frame_no));
        let laid_out = ctx.content_rect();
        *lock(&shared.egui_screen) = (laid_out.width(), laid_out.height());
        let repaint_delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [px.0, px.1],
            pixels_per_point: output.pixels_per_point,
        };

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("emacs-egui-native"),
            });
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
            }
        }
        let user_buffers =
            renderer.update_buffers(&gpu.device, &gpu.queue, &mut encoder, &primitives, &screen);
        {
            let target = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("emacs-egui-native"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer.render(&mut pass.forget_lifetime(), &primitives, &screen);
        }
        {
            let _submitting = lock(&gpu.submit_lock);
            gpu.queue
                .submit(user_buffers.into_iter().chain([encoder.finish()]));
            gpu.queue.present(frame);
        }
        for id in &output.textures_delta.free {
            renderer.free_texture(id);
        }

        // egui's own repaint request: an animation, a hover fade, a cursor blink.
        if repaint_delay < Duration::from_secs(3600) {
            let at = Instant::now() + repaint_delay;
            let mut wake = lock(&shared.wake);
            wake.repaint_at = Some(wake.repaint_at.map_or(at, |t| t.min(at)));
        }

        let n = shared.frames.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            let ms = shared.created_at.elapsed().as_secs_f64() * 1000.0;
            *lock(&shared.first_frame_ms) = Some(ms);
            emit(shared.id, "ready", &format!(":ms {ms:.1}"));
        }
        if let Some((at, by)) = lock(&shared.shown_at).take() {
            let ms = at.elapsed().as_secs_f64() * 1000.0;
            *lock(&shared.resume_ms) = Some(ms);
            emit(
                shared.id,
                "resumed",
                &format!(":ms {ms:.3} :by {}", lisp_string(by)),
            );
        }
        report_frames += 1;
        let since = report_at.elapsed();
        if since >= Duration::from_secs(1) {
            let fps = report_frames as f64 / since.as_secs_f64();
            *lock(&shared.fps) = fps;
            emit(shared.id, "frames", &format!(":count {n} :fps {fps:.1}"));
            report_at = Instant::now();
            report_frames = 0;
        }
    }
}

fn surface_config_defaults(
    surface: &wgpu::Surface<'static>,
    gpu: &Gpu,
    px: (u32, u32),
) -> wgpu::SurfaceConfiguration {
    surface
        .get_default_config(&gpu.adapter, px.0, px.1)
        .expect("Metal surface supported by its own device")
}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------
//
// A handle is a `user-ptr` whose pointer is the view, holding one reference
// to it, so that `make-xwidget' can read the view straight out of it. Its
// finalizer is `finalize_view`. `emacs-egui-native-release` stops the view and
// drops that reference at once, and then makes the pointer NULL, so the
// finalizer of a released handle has nothing to do and `make-xwidget' refuses
// it.

/// The finalizer of a view handle. Emacs runs finalizers during garbage
/// collection, which may happen on a Lisp thread; AppKit objects are released
/// on the main thread.
unsafe extern "C" fn finalize_view(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let addr = p as usize;
    let release = move || {
        let _ = panic::catch_unwind(|| unsafe { release_view(addr as *mut c_void) });
    };
    if MainThreadMarker::new().is_some() {
        release();
    } else {
        DispatchQueue::main().exec_async(release);
    }
}

/// Whether F is `finalize_view`, which marks a `user-ptr` as a view handle.
fn is_view_finalizer(f: unsafe extern "C" fn(*mut c_void)) -> bool {
    let ours: unsafe extern "C" fn(*mut c_void) = finalize_view;
    ptr::fn_addr_eq(f, ours)
}

/// Stop the view P points to and drop the reference a handle held to it.
/// Main thread only.
unsafe fn release_view(p: *mut c_void) {
    if let Some(view) = unsafe { Retained::from_raw(p as *mut EguiView) } {
        view.stop_render();
    }
}

/// `emacs-egui-native--clear-handle HANDLE`: make a view handle's pointer
/// NULL. Return t, or nil when HANDLE is not a view handle.
unsafe extern "C" fn clear_handle(
    env: *mut emacs_env,
    nargs: isize,
    args: *mut emacs_value,
    _data: *mut c_void,
) -> emacs_value {
    unsafe {
        let e = &*env;
        let intern = e.intern.expect("emacs_env.intern");
        let nil = intern(env, c"nil".as_ptr());
        if nargs != 1 {
            return nil;
        }
        let handle = *args;
        let fin = (e.get_user_finalizer.expect("emacs_env.get_user_finalizer"))(env, handle);
        if (e
            .non_local_exit_check
            .expect("emacs_env.non_local_exit_check"))(env)
            != 0
        {
            return nil;
        }
        if !fin.is_some_and(is_view_finalizer) {
            return nil;
        }
        (e.set_user_ptr.expect("emacs_env.set_user_ptr"))(env, handle, ptr::null_mut());
        intern(env, c"t".as_ptr())
    }
}

/// The view pointer of HANDLE, NULL once released; an error if HANDLE is not
/// a view handle.
fn handle_pointer(handle: Value) -> Result<*mut c_void> {
    let fin = handle.get_user_finalizer()?;
    if !fin.is_some_and(is_view_finalizer) {
        return Err(anyhow::anyhow!("not an emacs-egui-native view handle").into());
    }
    handle.get_user_ptr()
}

fn view_of(handle: Value) -> Result<Retained<EguiView>> {
    let p = handle_pointer(handle)? as *mut EguiView;
    // SAFETY: a handle that is not released holds a reference to its view.
    unsafe { Retained::retain(p) }
        .ok_or_else(|| anyhow::anyhow!("the view handle was released").into())
}

fn main_thread() -> Result<MainThreadMarker> {
    MainThreadMarker::new().ok_or_else(|| {
        anyhow::anyhow!("emacs-egui-native must be called on the main thread").into()
    })
}

fn number(v: Value) -> Result<f64> {
    if v.env.call("integerp", (v,))?.is_not_nil() {
        Ok(v.into_rust::<i64>()? as f64)
    } else {
        Ok(v.into_rust::<f64>()?)
    }
}

fn rect_list<'e>(env: &'e Env, r: NSRect) -> Result<Value<'e>> {
    env.list((r.origin.x, r.origin.y, r.size.width, r.size.height))
}

// ---------------------------------------------------------------------------
// Lisp functions
// ---------------------------------------------------------------------------

/// Route events from every view to PIPE-PROCESS, a process made by
/// `make-pipe-process`. Each event is one line, `(ID ACTION . PAYLOAD)`.
#[defun]
fn open_channel(env: &Env, pipe_process: Value) -> Result<()> {
    guard(|| {
        let mut writer = env.open_channel(pipe_process)?;
        let (tx, rx) = mpsc::channel::<String>();
        thread::Builder::new()
            .name("emacs-egui-native-events".into())
            .spawn(move || {
                for line in rx {
                    if writer.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                }
            })?;
        *lock(&CHANNEL) = Some(tx);
        Ok(())
    })
}

/// Create a Metal-backed view of WIDTH by HEIGHT points and start its render
/// thread. Return a handle for `(make-xwidget 'native TITLE WIDTH HEIGHT
/// HANDLE)`: a `user-ptr` whose pointer is the view. The view draws once
/// Emacs shows it.
#[defun]
fn make_view<'e>(env: &'e Env, width: Value, height: Value) -> Result<Value<'e>> {
    guard(|| {
        let created_at = Instant::now();
        let mtm = main_thread()?;
        let (w, h) = (number(width)?, number(height)?);
        let scale = NSScreen::mainScreen(mtm)
            .map(|s| s.backingScaleFactor())
            .unwrap_or(2.0);
        let gpu = gpu()?;

        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let shared = Arc::new(Shared::new(id, w, h, scale, created_at));
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
        let view = EguiView::new(mtm, frame, shared.clone());

        // Layer-hosting view: a plain root layer, and the Metal layer under it
        // with a delegate of ours (see LayerDelegate), sized by `resized`.
        let host = CALayer::new();
        view.setLayer(Some(&host));
        view.setWantsLayer(true);
        let layer = CAMetalLayer::new();
        layer.setFrame(frame);
        layer.setContentsScale(scale);
        layer.setOpaque(true);
        let delegate = LayerDelegate::new();
        layer.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        host.addSublayer(&layer);

        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                frame,
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking);

        let surface = unsafe {
            gpu.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                    Retained::as_ptr(&layer) as *mut c_void,
                ))
        }
        .map_err(|e| anyhow::anyhow!("cannot make a wgpu surface: {e}"))?;

        let thread_shared = shared.clone();
        let thread_layer = SendLayer(layer.clone());
        let thread = thread::Builder::new()
            .name(format!("emacs-egui-native-{id}"))
            .spawn(move || {
                LIVE_THREADS.fetch_add(1, Ordering::SeqCst);
                let id = thread_shared.id;
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    render_loop(thread_shared, gpu, surface, thread_layer)
                }));
                if let Err(e) = result {
                    let msg = take_panic_message(&*e);
                    emit(id, "error", &format!(":message {}", lisp_string(&msg)));
                }
                LIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
            })?;
        *view.ivars().render.borrow_mut() = Some(Render {
            layer,
            _delegate: delegate,
            thread,
        });

        let p = Retained::into_raw(view) as *mut c_void;
        unsafe { env.make_user_ptr(Some(finalize_view), p) }
    })
}

/// Drop the module's reference to HANDLE's view and stop its render thread.
/// An xwidget that holds the view keeps its own reference, and the view stays
/// where Emacs put it, showing its last frame, until the xwidget is killed.
/// Releasing a handle twice does nothing.
#[defun]
fn release(env: &Env, handle: Value) -> Result<()> {
    guard(|| {
        main_thread()?;
        let p = handle_pointer(handle)?;
        if p.is_null() {
            return Ok(());
        }
        env.call("emacs-egui-native--clear-handle", (handle,))?;
        unsafe { release_view(p) };
        Ok(())
    })
}

/// Hand STATE, a plist, to the view's UI. The label shows its `:label`; a
/// non-nil `:animate` keeps a bar moving, which asks for a frame every vsync.
#[defun]
fn push(env: &Env, handle: Value, state: Value) -> Result<()> {
    guard(|| {
        let view = view_of(handle)?;
        let mut pairs = Vec::new();
        let mut rest = state;
        while rest.is_not_nil() {
            let key: Value = rest.car()?;
            let tail: Value = rest.cdr()?;
            let value: Value = tail.car()?;
            let key = env.call("symbol-name", (key,))?.into_rust::<String>()?;
            let printed = if env.call("stringp", (value,))?.is_not_nil() {
                value.into_rust::<String>()?
            } else {
                env.call("prin1-to-string", (value,))?
                    .into_rust::<String>()?
            };
            pairs.push((key, printed));
            rest = tail.cdr()?;
        }
        let shared = &view.ivars().shared;
        let animate = pairs.iter().any(|(k, v)| k == ":animate" && v != "nil");
        shared.animate.store(animate, Ordering::SeqCst);
        *lock(&shared.state) = pairs;
        shared.request_frame();
        Ok(())
    })
}

/// Let the view take keys: the egui text field gets them, except `C-g`.
#[defun]
fn focus(handle: Value) -> Result<()> {
    guard(|| {
        main_thread()?;
        view_of(handle)?.set_focused(true);
        Ok(())
    })
}

/// Give keys back to Emacs.
#[defun]
fn unfocus(handle: Value) -> Result<()> {
    guard(|| {
        main_thread()?;
        view_of(handle)?.set_focused(false);
        Ok(())
    })
}

/// The number of frames the view has rendered.
#[defun]
fn frames(handle: Value) -> Result<i64> {
    guard(|| {
        Ok(view_of(handle)?
            .ivars()
            .shared
            .frames
            .load(Ordering::SeqCst) as i64)
    })
}

/// The view's numeric id, the first element of each of its event lines.
#[defun]
fn id(handle: Value) -> Result<i64> {
    guard(|| Ok(view_of(handle)?.ivars().shared.id as i64))
}

/// A plist describing the view, for tests and measures. Geometry is in
/// points: `:frame` in its container, `:in-emacs-view` in the `EmacsView`,
/// `:visible-rect` the part AppKit reports unclipped by ancestors.
#[defun]
fn info<'e>(env: &'e Env, handle: Value) -> Result<Value<'e>> {
    guard(|| {
        let mtm = main_thread()?;
        let view = view_of(handle)?;
        let s = &view.ivars().shared;
        let rect = |r: Option<egui::Rect>| -> Result<Value<'e>> {
            match r {
                Some(r) => env.list((
                    r.left() as f64,
                    r.top() as f64,
                    r.width() as f64,
                    r.height() as f64,
                )),
                None => ().into_lisp(env),
            }
        };
        let nil = || ().into_lisp(env);
        let rects = *lock(&s.rects);
        let geo = *lock(&s.geometry);
        let (egui_w, egui_h) = *lock(&s.egui_screen);
        let render = view.ivars().render.borrow();
        let (layer, drawable) = match render.as_ref() {
            Some(r) => {
                let b = r.layer.bounds();
                let d = r.layer.drawableSize();
                (
                    env.list((b.size.width, b.size.height))?,
                    env.list((d.width, d.height))?,
                )
            }
            None => (nil()?, nil()?),
        };
        let emacs = view.emacs_view();
        let in_emacs = match emacs.as_deref() {
            Some(e) => rect_list(env, view.convertRect_toView(view.bounds(), Some(e)))?,
            None => nil()?,
        };
        // SAFETY: as in `emacs_view`.
        let container = unsafe { view.superview().and_then(|c| c.superview()) };
        let (container_frame, container_clips) = match container.as_deref() {
            Some(c) => (
                rect_list(env, c.frame())?,
                c.clipsToBounds().into_lisp(env)?,
            ),
            None => (nil()?, nil()?),
        };
        let window = view.window();
        let first_responder = match window.as_deref().and_then(|w| w.firstResponder()) {
            Some(r) => r
                .class()
                .name()
                .to_string_lossy()
                .into_owned()
                .into_lisp(env)?,
            None => nil()?,
        };
        let key_window = NSApplication::sharedApplication(mtm)
            .keyWindow()
            .zip(window.as_deref())
            .is_some_and(|(k, w)| ptr::eq(&*k, w));
        let first = *lock(&s.first_frame_ms);
        let vals: Vec<Value> = vec![
            env.intern(":id")?,
            (s.id as i64).into_lisp(env)?,
            env.intern(":rendering")?,
            render.is_some().into_lisp(env)?,
            env.intern(":visible")?,
            s.visible.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":animate")?,
            s.animate.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":frames")?,
            (s.frames.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":fps")?,
            (*lock(&s.fps)).into_lisp(env)?,
            env.intern(":hides")?,
            (s.hides.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":unhides")?,
            (s.unhides.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":pause-ms")?,
            (*lock(&s.pause_ms)).into_lisp(env)?,
            env.intern(":resume-ms")?,
            (*lock(&s.resume_ms)).into_lisp(env)?,
            env.intern(":clicks")?,
            (s.clicks.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":text")?,
            lock(&s.text).clone().into_lisp(env)?,
            env.intern(":focused")?,
            s.focused.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":text-focused")?,
            s.text_focused.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":first-frame-ms")?,
            first.into_lisp(env)?,
            env.intern(":button")?,
            rect(rects.button)?,
            env.intern(":text-field")?,
            rect(rects.text)?,
            env.intern(":scale")?,
            (geo.scale as f64).into_lisp(env)?,
            env.intern(":format")?,
            lock(&s.surface_format).clone().into_lisp(env)?,
            env.intern(":frame")?,
            rect_list(env, view.frame())?,
            env.intern(":in-emacs-view")?,
            in_emacs,
            env.intern(":visible-rect")?,
            rect_list(env, view.visibleRect())?,
            env.intern(":hidden-or-ancestor")?,
            view.isHiddenOrHasHiddenAncestor().into_lisp(env)?,
            env.intern(":in-window")?,
            window.is_some().into_lisp(env)?,
            env.intern(":container")?,
            container_frame,
            env.intern(":container-clips")?,
            container_clips,
            env.intern(":layer")?,
            layer,
            env.intern(":drawable")?,
            drawable,
            env.intern(":egui-screen")?,
            env.list((egui_w as f64, egui_h as f64))?,
            env.intern(":first-responder")?,
            first_responder,
            env.intern(":key-window")?,
            key_window.into_lisp(env)?,
        ];
        env.call("list", &vals[..])
    })
}

/// Panic inside a module function, on purpose.
#[defun(name = "panic")]
fn deliberate_panic() -> Result<()> {
    guard(|| {
        panic!("emacs-egui-native-panic: deliberate panic inside a module function");
    })
}

/// Test entry: click the view's button, or its text field when TARGET is
/// `text`. Without DIRECT, a mouse-down and a mouse-up are posted to the
/// application's event queue, so AppKit routes them by hit-testing when Emacs
/// next runs its event loop. With DIRECT, they are handed to the view's
/// `mouseDown:` and `mouseUp:` at once. Return the point clicked, in window
/// coordinates.
#[defun]
fn test_click<'e>(
    env: &'e Env,
    handle: Value,
    direct: Option<Value>,
    target: Option<Value>,
) -> Result<Value<'e>> {
    guard(|| {
        let mtm = main_thread()?;
        let view = view_of(handle)?;
        let text = match target {
            Some(t) => env.call("eq", (t, env.intern("text")?))?.is_not_nil(),
            None => false,
        };
        let rects = *lock(&view.ivars().shared.rects);
        let rect = if text { rects.text } else { rects.button }
            .ok_or_else(|| anyhow::anyhow!("the view has not been drawn yet"))?;
        let center = rect.center();
        let window = view
            .window()
            .ok_or_else(|| anyhow::anyhow!("view has no window"))?;
        let p = view.convertPoint_toView(NSPoint::new(center.x as f64, center.y as f64), None);
        let now = NSProcessInfo::processInfo().systemUptime();
        let make = |kind: NSEventType, t: f64| {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                kind,
                p,
                NSEventModifierFlags::empty(),
                t,
                window.windowNumber(),
                None,
                0,
                1,
                if kind == NSEventType::LeftMouseDown { 1.0 } else { 0.0 },
            )
            .ok_or_else(|| anyhow::anyhow!("cannot make a mouse event"))
        };
        let down = make(NSEventType::LeftMouseDown, now)?;
        let up = make(NSEventType::LeftMouseUp, now + 0.05)?;
        let direct = direct.is_some_and(|d| d.is_not_nil());
        if direct {
            unsafe {
                let _: () = msg_send![&*view, mouseDown: &*down];
                let _: () = msg_send![&*view, mouseUp: &*up];
            }
        } else {
            let app = NSApplication::sharedApplication(mtm);
            app.postEvent_atStart(&down, false);
            app.postEvent_atStart(&up, false);
        }
        env.list((p.x, p.y, if direct { "direct" } else { "posted" }))
    })
}

/// Test entry: post a key-down and a key-up for CHARS, one letter, to the
/// application's event queue, addressed to the view's window. CTRL non-nil adds
/// Control. AppKit delivers it to the window's first responder when Emacs next
/// runs its event loop, as it would a typed key.
#[defun]
fn test_post_key(handle: Value, chars: String, ctrl: Option<Value>) -> Result<()> {
    guard(|| {
        let mtm = main_thread()?;
        let view = view_of(handle)?;
        let window = view
            .window()
            .ok_or_else(|| anyhow::anyhow!("view has no window"))?;
        let c = chars
            .chars()
            .next()
            .ok_or_else(|| anyhow::anyhow!("CHARS is empty"))?;
        let ctrl = ctrl.is_some_and(|v| v.is_not_nil());
        let typed = if ctrl {
            char::from_u32((c.to_ascii_lowercase() as u32) & 0x1f)
                .unwrap_or(c)
                .to_string()
        } else {
            c.to_string()
        };
        let flags = if ctrl {
            NSEventModifierFlags::Control
        } else {
            NSEventModifierFlags::empty()
        };
        let now = NSProcessInfo::processInfo().systemUptime();
        let app = NSApplication::sharedApplication(mtm);
        for (kind, t) in [
            (NSEventType::KeyDown, now),
            (NSEventType::KeyUp, now + 0.02),
        ] {
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                kind,
                NSPoint::new(0.0, 0.0),
                flags,
                t,
                window.windowNumber(),
                None,
                &NSString::from_str(&typed),
                &NSString::from_str(&c.to_ascii_lowercase().to_string()),
                false,
                letter_key_code(c),
            )
            .ok_or_else(|| anyhow::anyhow!("cannot make a key event"))?;
            app.postEvent_atStart(&event, false);
        }
        Ok(())
    })
}

/// Test entry: `(:views N :threads N :in-windows N)`, the views not yet
/// deallocated, the render threads still running, and the views found in the
/// view hierarchy of any of the application's windows.
#[defun]
fn test_counts<'e>(env: &'e Env) -> Result<Value<'e>> {
    guard(|| {
        let mtm = main_thread()?;
        fn count(view: &NSView) -> i64 {
            let here = i64::from(view.isKindOfClass(EguiView::class()));
            here + view.subviews().iter().map(|v| count(&v)).sum::<i64>()
        }
        let in_windows: i64 = NSApplication::sharedApplication(mtm)
            .windows()
            .iter()
            .filter_map(|w| w.contentView())
            .map(|v| count(&v))
            .sum();
        env.list((
            env.intern(":views")?,
            LIVE_VIEWS.load(Ordering::SeqCst),
            env.intern(":threads")?,
            LIVE_THREADS.load(Ordering::SeqCst),
            env.intern(":in-windows")?,
            in_windows,
        ))
    })
}

/// Test entry: make the application inactive, so no window of it is key, as
/// when the user works in another application.
#[defun]
fn test_deactivate() -> Result<()> {
    guard(|| {
        let mtm = main_thread()?;
        NSApplication::sharedApplication(mtm).deactivate();
        Ok(())
    })
}
