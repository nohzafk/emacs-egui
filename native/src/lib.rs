//! Native backend of emacs-egui: a dynamic module that renders egui with Metal
//! into an `NSView` it creates inside the Emacs process.
//!
//! Threads:
//! - Every `emacs-egui-native-*` function runs on the Emacs main thread, which
//!   is also the AppKit main thread. All AppKit objects are touched there.
//! - Each view has a render thread of its own. It owns the egui context and the
//!   wgpu surface, and draws into a `CAMetalLayer` without the main thread, so
//!   it keeps drawing while Elisp is busy and Emacs is not pumping the run loop.
//! - One writer thread drains events into the pipe process that Elisp opened
//!   with `emacs-egui-native-open-channel`, so a full pipe never stalls a frame.
//!
//! Window lookup by title (`emacs-egui-native-attach`) is the N0 scaffold; the
//! `native` xwidget type replaces it (docs/design/100-embedded-widgets.md, N1).

use std::{
    cell::RefCell,
    ffi::c_void,
    io::Write,
    panic::{self, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use dispatch2::{DispatchQueue, MainThreadBound};
use egui_wgpu::wgpu;
use emacs::{defun, Env, IntoLisp, Result, Transfer, Value};
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message,
};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSEvent, NSEventModifierFlags, NSEventType,
    NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
};
use objc2_foundation::{
    NSObject, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize, NSString,
};
use objc2_quartz_core::{CAAutoresizingMask, CALayer, CALayerDelegate, CAMetalLayer};

emacs::plugin_is_GPL_compatible!();

#[emacs::module(
    name = "emacs-egui-native",
    defun_prefix = "emacs-egui-native",
    separator = "-"
)]
fn init(_: &Env) -> Result<()> {
    install_panic_hook();
    Ok(())
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
    if let Ok(guard) = CHANNEL.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(line);
        }
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

struct Shared {
    id: u64,
    running: AtomicBool,
    focused: AtomicBool,
    frames: AtomicU64,
    clicks: AtomicU64,
    input: Mutex<Vec<egui::Event>>,
    /// The last pushed plist, as (key, printed value) pairs.
    state: Mutex<Vec<(String, String)>>,
    text: Mutex<String>,
    rects: Mutex<Rects>,
    geometry: Mutex<Geometry>,
    attached_at: Instant,
    first_frame_ms: Mutex<Option<f64>>,
    fps: Mutex<f64>,
    surface_format: Mutex<String>,
    /// Whether egui's text field held keyboard focus in the last frame.
    text_focused: AtomicBool,
}

impl Shared {
    fn push_input(&self, event: egui::Event) {
        self.input.lock().unwrap().push(event);
    }

    fn label(&self) -> String {
        let state = self.state.lock().unwrap();
        state
            .iter()
            .find(|(k, _)| k == ":label")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "(no :label pushed)".to_string())
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

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
            std::ptr::null_mut()
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

struct ViewIvars {
    shared: Arc<Shared>,
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

        /// Only a focused view takes keys (D4); unfocused, AppKit never makes
        /// it first responder, so keys stay with Emacs.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            self.ivars().shared.focused.load(Ordering::SeqCst)
        }

        /// Mouse events reach egui even though the hosting window is not key.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.pointer_button(event, egui::PointerButton::Primary, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.pointer_button(event, egui::PointerButton::Primary, false);
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

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.key(event, true);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.key(event, false);
        }

        /// As in nsxwidget: do nothing, so the input manager never turns a key
        /// into text behind the view's back.
        #[unsafe(method(interpretKeyEvents:))]
        fn interpret_key_events(&self, _events: &AnyObject) {}
    }

    unsafe impl NSObjectProtocol for EguiView {}
);

impl EguiView {
    fn new(mtm: MainThreadMarker, frame: NSRect, shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { shared });
        unsafe { msg_send![super(this), initWithFrame: frame] }
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

    /// The keyboard policy (D4). Unfocused: every key goes to Emacs. Focused:
    /// keys go to egui, except `C-g`, which unfocuses and goes to Emacs.
    fn key(&self, event: &NSEvent, down: bool) {
        let shared = self.ivars().shared.clone();
        let flags = event.modifierFlags();
        let unmodified = event
            .charactersIgnoringModifiers()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let is_quit = flags.contains(NSEventModifierFlags::Control)
            && !flags.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Option)
            && unmodified.eq_ignore_ascii_case("g");
        let focused = shared.focused.load(Ordering::SeqCst);

        if focused && !is_quit {
            for e in egui_key_events(event, down) {
                shared.push_input(e);
            }
            return;
        }
        if !down {
            return;
        }
        if focused && is_quit {
            self.set_focused(false);
            emit(shared.id, "unfocus", ":reason quit");
        }
        self.forward_key(event);
    }

    /// The `EmacsView` that receives this view's keys: the parent frame's when
    /// the view sits in a child frame (a no-accept-focus child frame never gets
    /// keys of its own), else the hosting frame's.
    fn emacs_target(&self) -> Option<(Retained<NSWindow>, Retained<NSView>)> {
        let window = self.window()?;
        let target = window.parentWindow().unwrap_or(window);
        let view = emacs_view_of(&target)?;
        Some((target, view))
    }

    fn forward_key(&self, event: &NSEvent) {
        let Some((window, view)) = self.emacs_target() else {
            return;
        };
        // Re-target the event to the Emacs window, so EmacsView does not bounce
        // it to the window the event names.
        let chars = event.characters().unwrap_or_else(|| NSString::from_str(""));
        let unmodified = event
            .charactersIgnoringModifiers()
            .unwrap_or_else(|| NSString::from_str(""));
        let forwarded = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyDown,
            NSPoint::new(0.0, 0.0),
            event.modifierFlags(),
            event.timestamp(),
            window.windowNumber(),
            None,
            &chars,
            &unmodified,
            event.isARepeat(),
            event.keyCode(),
        );
        if let Some(forwarded) = forwarded {
            unsafe {
                let _: () = msg_send![&*view, keyDown: &*forwarded];
            }
        }
    }

    fn set_focused(&self, focused: bool) {
        self.ivars().shared.focused.store(focused, Ordering::SeqCst);
        let Some(window) = self.window() else {
            return;
        };
        if focused {
            window.makeFirstResponder(Some(self));
        } else if let Some(first) = window.firstResponder() {
            let me: &NSResponder = self;
            if std::ptr::eq(&*first, me) {
                let emacs = emacs_view_of(&window);
                window.makeFirstResponder(emacs.as_deref().map(|v| -> &NSResponder { v }));
            }
        }
    }
}

/// The window's `EmacsView`, which Emacs installs as the window's delegate.
fn emacs_view_of(window: &NSWindow) -> Option<Retained<NSView>> {
    let delegate = window.delegate()?;
    let obj: &AnyObject = delegate.as_ref();
    let view = obj.downcast_ref::<NSView>()?;
    if view.class().name().to_str().ok()? != "EmacsView" {
        return None;
    }
    Some(view.retain())
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
                *shared.text.lock().unwrap() = st.text.clone();
                emit(
                    shared.id,
                    "text",
                    &format!(":value {}", lisp_string(&st.text)),
                );
            }
            // A bar that crosses the track once a second.
            let (track, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(track, 3.0, egui::Color32::from_gray(60));
            let t = (st.start.elapsed().as_secs_f32() % 1.0) as f32;
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
            *shared.rects.lock().unwrap() = rects;
        });
}

/// Take pending input for one frame. At most one pointer-button transition per
/// frame, so a press and a release delivered together still reach egui in two
/// passes, as a real click does.
///
/// Keys wait while the view is focused but egui has not yet given the text
/// field focus: egui grants a `request_focus` one frame later, and a key in
/// between would be dropped.
fn take_input(shared: &Shared) -> Vec<egui::Event> {
    let hold_keys =
        shared.focused.load(Ordering::SeqCst) && !shared.text_focused.load(Ordering::SeqCst);
    let mut queue = shared.input.lock().unwrap();
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
    *queue = kept;
    taken
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
    *shared.surface_format.lock().unwrap() = format!("{format:?}");
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

    while shared.running.load(Ordering::SeqCst) {
        let geo = *shared.geometry.lock().unwrap();
        let px = (
            (geo.width * geo.scale).round().max(1.0) as u32,
            (geo.height * geo.scale).round().max(1.0) as u32,
        );
        if configured != Some(px) {
            let _submitting = gpu.submit_lock.lock().unwrap();
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
            // block `nextDrawable` forever and `detach` could not join us.
            layer.0.setAllowsNextDrawableTimeout(true);
            configured = Some(px);
        }

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                configured = None;
                continue;
            }
            _ => {
                thread::sleep(Duration::from_millis(16));
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
            let _submitting = gpu.submit_lock.lock().unwrap();
            gpu.queue
                .submit(user_buffers.into_iter().chain([encoder.finish()]));
            gpu.queue.present(frame);
        }
        for id in &output.textures_delta.free {
            renderer.free_texture(id);
        }

        let n = shared.frames.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            let ms = shared.attached_at.elapsed().as_secs_f64() * 1000.0;
            *shared.first_frame_ms.lock().unwrap() = Some(ms);
            emit(shared.id, "ready", &format!(":ms {ms:.1}"));
        }
        report_frames += 1;
        let since = report_at.elapsed();
        if since >= Duration::from_secs(1) {
            let fps = report_frames as f64 / since.as_secs_f64();
            *shared.fps.lock().unwrap() = fps;
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

struct Attached {
    view: Retained<EguiView>,
    layer: Retained<CAMetalLayer>,
    _delegate: Retained<LayerDelegate>,
    thread: JoinHandle<()>,
}

pub struct Handle {
    shared: Arc<Shared>,
    attached: RefCell<Option<Attached>>,
}

impl Transfer for Handle {
    fn type_name() -> &'static str {
        "emacs-egui-native-view"
    }
}

impl Handle {
    fn view(&self) -> Result<Retained<EguiView>> {
        self.attached
            .borrow()
            .as_ref()
            .map(|a| a.view.clone())
            .ok_or_else(|| anyhow::anyhow!("view {} is detached", self.shared.id))
    }

    fn detach(&self) {
        let Some(attached) = self.attached.borrow_mut().take() else {
            return;
        };
        self.shared.running.store(false, Ordering::SeqCst);
        let _ = attached.thread.join();
        attached.view.set_focused(false);
        attached.view.removeFromSuperview();
        attached.layer.setDelegate(None);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.detach();
    }
}

fn main_thread() -> Result<MainThreadMarker> {
    MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("emacs-egui-native must be called on the main thread"))
}

fn number(v: Value) -> Result<f64> {
    if v.env.call("integerp", (v,))?.is_not_nil() {
        Ok(v.into_rust::<i64>()? as f64)
    } else {
        Ok(v.into_rust::<f64>()?)
    }
}

fn find_window(mtm: MainThreadMarker, title: &str) -> Result<Retained<NSWindow>> {
    let app = NSApplication::sharedApplication(mtm);
    let windows = app.windows();
    let matches: Vec<Retained<NSWindow>> = windows
        .iter()
        .filter(|w| w.title().to_string() == title)
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("one")),
        0 => {
            let titles: Vec<String> = windows.iter().map(|w| w.title().to_string()).collect();
            Err(anyhow::anyhow!(
                "no NSWindow titled {title:?}; titles: {titles:?}"
            ))
        }
        n => Err(anyhow::anyhow!(
            "{n} NSWindows titled {title:?}; the title must be unique"
        )),
    }
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
        *CHANNEL.lock().unwrap() = Some(tx);
        Ok(())
    })
}

/// Create a Metal-backed view in the window titled WINDOW-TITLE, at X Y with
/// size W H in the window content's pixels (points), and start its render
/// thread. Return a handle.
#[defun(user_ptr)]
fn attach(window_title: String, x: Value, y: Value, w: Value, h: Value) -> Result<Handle> {
    guard(|| {
        let attached_at = Instant::now();
        let mtm = main_thread()?;
        let (x, y, w, h) = (number(x)?, number(y)?, number(w)?, number(h)?);
        let window = find_window(mtm, &window_title)?;
        let content = window
            .contentView()
            .ok_or_else(|| anyhow::anyhow!("window has no content view"))?;
        let scale = window.backingScaleFactor();
        let gpu = gpu()?;

        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let shared = Arc::new(Shared {
            id,
            running: AtomicBool::new(true),
            focused: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            clicks: AtomicU64::new(0),
            input: Mutex::new(Vec::new()),
            state: Mutex::new(Vec::new()),
            text: Mutex::new(String::new()),
            rects: Mutex::new(Rects::default()),
            geometry: Mutex::new(Geometry {
                width: w as f32,
                height: h as f32,
                scale: scale as f32,
            }),
            attached_at,
            first_frame_ms: Mutex::new(None),
            fps: Mutex::new(0.0),
            surface_format: Mutex::new(String::new()),
            text_focused: AtomicBool::new(false),
        });

        let flipped = content.isFlipped();
        let content_h = content.frame().size.height;
        let origin_y = if flipped { y } else { content_h - y - h };
        let view = EguiView::new(
            mtm,
            NSRect::new(NSPoint::new(x, origin_y), NSSize::new(w, h)),
            shared.clone(),
        );
        if !flipped {
            view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinYMargin);
        }

        // Layer-hosting view: a plain root layer that AppKit sizes with the view,
        // and the Metal layer under it with a delegate of ours (see LayerDelegate).
        let host = CALayer::new();
        view.setLayer(Some(&host));
        view.setWantsLayer(true);
        let layer = CAMetalLayer::new();
        layer.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)));
        layer.setContentsScale(scale);
        layer.setAutoresizingMask(
            CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
        );
        layer.setOpaque(true);
        let delegate = LayerDelegate::new();
        layer.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        host.addSublayer(&layer);

        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h)),
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&view),
                None,
            )
        };
        view.addTrackingArea(&tracking);
        content.addSubview(&view);

        let surface = unsafe {
            gpu.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                    Retained::as_ptr(&layer) as *mut c_void,
                ))
        };
        let surface = match surface {
            Ok(s) => s,
            Err(e) => {
                view.removeFromSuperview();
                return Err(anyhow::anyhow!("cannot make a wgpu surface: {e}"));
            }
        };

        let thread_shared = shared.clone();
        let thread_layer = SendLayer(layer.clone());
        let thread = thread::Builder::new()
            .name(format!("emacs-egui-native-{id}"))
            .spawn(move || {
                let id = thread_shared.id;
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    render_loop(thread_shared, gpu, surface, thread_layer)
                }));
                if let Err(e) = result {
                    let msg = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "render thread panicked".to_string());
                    emit(id, "error", &format!(":message {}", lisp_string(&msg)));
                }
            })?;

        Ok(Handle {
            shared,
            attached: RefCell::new(Some(Attached {
                view,
                layer,
                _delegate: delegate,
                thread,
            })),
        })
    })
}

/// Hand STATE, a plist, to the view's UI. The label shows its `:label`.
#[defun]
fn push(env: &Env, handle: &Handle, state: Value) -> Result<()> {
    guard(|| {
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
        *handle.shared.state.lock().unwrap() = pairs;
        Ok(())
    })
}

/// Let the view take keys: the egui text field gets them, except `C-g`.
#[defun]
fn focus(handle: &Handle) -> Result<()> {
    guard(|| {
        main_thread()?;
        handle.view()?.set_focused(true);
        Ok(())
    })
}

/// Give keys back to Emacs.
#[defun]
fn unfocus(handle: &Handle) -> Result<()> {
    guard(|| {
        main_thread()?;
        handle.view()?.set_focused(false);
        Ok(())
    })
}

/// Remove the view and stop its render thread.
#[defun]
fn detach(handle: &Handle) -> Result<()> {
    guard(|| {
        main_thread()?;
        handle.detach();
        Ok(())
    })
}

/// The number of frames the view has rendered.
#[defun]
fn frames(handle: &Handle) -> Result<i64> {
    guard(|| Ok(handle.shared.frames.load(Ordering::SeqCst) as i64))
}

/// The view's numeric id, the first element of each of its event lines.
#[defun]
fn id(handle: &Handle) -> Result<i64> {
    guard(|| Ok(handle.shared.id as i64))
}

/// A plist describing the view, for tests and measures.
#[defun]
fn info<'e>(env: &'e Env, handle: &Handle) -> Result<Value<'e>> {
    guard(|| {
        let s = &handle.shared;
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
        let rects = *s.rects.lock().unwrap();
        let geo = *s.geometry.lock().unwrap();
        let first = *s.first_frame_ms.lock().unwrap();
        let vals: Vec<Value> = vec![
            env.intern(":id")?,
            (s.id as i64).into_lisp(env)?,
            env.intern(":attached")?,
            handle.attached.borrow().is_some().into_lisp(env)?,
            env.intern(":frames")?,
            (s.frames.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":fps")?,
            (*s.fps.lock().unwrap()).into_lisp(env)?,
            env.intern(":clicks")?,
            (s.clicks.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":text")?,
            s.text.lock().unwrap().clone().into_lisp(env)?,
            env.intern(":focused")?,
            s.focused.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":first-frame-ms")?,
            first.into_lisp(env)?,
            env.intern(":button")?,
            rect(rects.button)?,
            env.intern(":text-field")?,
            rect(rects.text)?,
            env.intern(":scale")?,
            (geo.scale as f64).into_lisp(env)?,
            env.intern(":format")?,
            s.surface_format.lock().unwrap().clone().into_lisp(env)?,
            env.intern(":text-focused")?,
            s.text_focused.load(Ordering::SeqCst).into_lisp(env)?,
        ];
        env.call("list", &vals[..])
    })
}

/// Titles of every NSWindow of the application, for finding a frame's window.
#[defun]
fn window_titles<'e>(env: &'e Env) -> Result<Value<'e>> {
    guard(|| {
        let mtm = main_thread()?;
        let app = NSApplication::sharedApplication(mtm);
        let mut vals = Vec::new();
        for w in app.windows().iter() {
            vals.push(w.title().to_string().into_lisp(env)?);
        }
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

/// Test entry: click the view's button. Without DIRECT, a mouse-down and a
/// mouse-up are posted to the application's event queue, so AppKit routes them
/// by hit-testing when Emacs next runs its event loop. With DIRECT, they are
/// handed to the view's `mouseDown:` and `mouseUp:` at once. Return the point,
/// in window coordinates, that was clicked.
#[defun]
fn test_click<'e>(env: &'e Env, handle: &Handle, direct: Option<Value>) -> Result<Value<'e>> {
    guard(|| {
        let mtm = main_thread()?;
        let view = handle.view()?;
        let button = handle
            .shared
            .rects
            .lock()
            .unwrap()
            .button
            .ok_or_else(|| anyhow::anyhow!("the button has not been drawn yet"))?;
        let center = button.center();
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
        let direct = direct.is_some();
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

/// Test entry: deliver a key to the view's `keyDown:`, as AppKit would to a
/// first responder. CHARS is one character; CTRL non-nil adds Control.
/// Delivery is scheduled on the main dispatch queue, which Emacs drains while
/// it waits for input in `ns_select`: `EmacsView` only accepts a key there,
/// so a key forwarded to Emacs from inside this call would be dropped.
#[defun]
fn test_key(handle: &Handle, chars: String, ctrl: Option<Value>) -> Result<()> {
    guard(|| deliver_test_key(handle, &chars, ctrl.is_some(), false))
}

/// Test entry, the control for `test-key`: deliver the same key straight to
/// the `EmacsView` that the view forwards keys to, bypassing the view.
#[defun]
fn test_emacs_key(handle: &Handle, chars: String, ctrl: Option<Value>) -> Result<()> {
    guard(|| deliver_test_key(handle, &chars, ctrl.is_some(), true))
}

fn deliver_test_key(handle: &Handle, chars: &str, ctrl: bool, bypass_view: bool) -> Result<()> {
    let mtm = main_thread()?;
    let view = MainThreadBound::new(handle.view()?, mtm);
    let c = chars
        .chars()
        .next()
        .ok_or_else(|| anyhow::anyhow!("CHARS is empty"))?;
    DispatchQueue::main().exec_async(move || {
            let Some(mtm) = MainThreadMarker::new() else {
                return;
            };
            let view = view.get(mtm);
            let target = if bypass_view {
                view.emacs_target()
            } else {
                view.window().map(|w| (w, Retained::into_super(view.clone())))
            };
            let Some((window, target)) = target else {
                return;
            };
            let typed = if ctrl {
                char::from_u32((c.to_ascii_lowercase() as u32) & 0x1f).unwrap_or(c).to_string()
            } else {
                c.to_string()
            };
            let flags = if ctrl { NSEventModifierFlags::Control } else { NSEventModifierFlags::empty() };
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyDown,
                NSPoint::new(0.0, 0.0),
                flags,
                NSProcessInfo::processInfo().systemUptime(),
                window.windowNumber(),
                None,
                &NSString::from_str(&typed),
                &NSString::from_str(&c.to_ascii_lowercase().to_string()),
                false,
                letter_key_code(c),
            );
            if let Some(event) = event {
                let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
                    let _: () = msg_send![&*target, keyDown: &*event];
                }));
            }
    });
    Ok(())
}
