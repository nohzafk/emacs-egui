//! What a view does in AppKit. The view class itself is defined in the
//! application crate by `export_app!` (Objective-C class names are global to
//! the process, and each application module needs classes of its own); its
//! methods call the functions here, which are generic over that class.
//!
//! Every function here runs on the main thread, where AppKit calls the
//! view's methods and where Emacs calls the module's functions.

use std::{
    cell::RefCell,
    panic::{self, AssertUnwindSafe},
    ptr::{self, NonNull},
    sync::{atomic::Ordering, Arc},
    thread::JoinHandle,
};

use block2::RcBlock;
use objc2::{
    msg_send,
    rc::{Retained, Weak},
    runtime::{AnyClass, AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSResponder, NSView, NSWindow,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayerDelegate, CAMetalLayer, CATransaction};

use crate::{
    convert::lisp_string,
    events::emit,
    lock,
    render::{Shared, LIVE_VIEWS},
    ViewClass,
};

/// What a view owns while it renders; taken and dropped by `stop_render`.
pub(crate) struct Render {
    pub(crate) layer: Retained<CAMetalLayer>,
    pub(crate) _delegate: Retained<ProtocolObject<dyn CALayerDelegate>>,
    pub(crate) thread: JoinHandle<()>,
}

/// The instance variables of an application's view class.
pub struct ViewIvars {
    pub(crate) shared: Arc<Shared>,
    pub(crate) render: RefCell<Option<Render>>,
}

impl ViewIvars {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        LIVE_VIEWS.fetch_add(1, Ordering::SeqCst);
        Self {
            shared,
            render: RefCell::new(None),
        }
    }
}

impl Drop for ViewIvars {
    fn drop(&mut self) {
        LIVE_VIEWS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The `EmacsView` of the frame the view is shown in: the first ancestor of
/// that class (nsxwidget puts the view in a container in a container in it).
pub(crate) fn emacs_view(view: &NSView) -> Option<Retained<NSView>> {
    let class = AnyClass::get(c"EmacsView")?;
    // SAFETY: on the main thread, between AppKit calls, a superview is
    // alive; the walk retains each one it returns.
    let mut next = unsafe { view.superview() };
    while let Some(v) = next {
        if v.isKindOfClass(class) {
            return Some(v);
        }
        next = unsafe { v.superview() };
    }
    None
}

pub(crate) fn is_first_responder(view: &NSView, window: &NSWindow) -> bool {
    let me: &NSResponder = view;
    window
        .firstResponder()
        .is_some_and(|first| ptr::eq(&*first, me))
}

fn local_pos(view: &NSView, event: &NSEvent) -> egui::Pos2 {
    let p = view.convertPoint_fromView(event.locationInWindow(), None);
    egui::pos2(p.x as f32, p.y as f32)
}

fn pointer_button<V: ViewClass>(
    this: &V,
    event: &NSEvent,
    button: egui::PointerButton,
    pressed: bool,
) {
    let pos = local_pos(this, event);
    let shared = &this.ivars().shared;
    shared.push_input(egui::Event::PointerMoved(pos));
    shared.push_input(egui::Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: modifiers(event.modifierFlags()),
    });
}

/// To egui, and to the `EmacsView` too, as nsxwidget does, so a click selects
/// the Emacs window the widget is in. Then the view takes the keys, which it
/// passes on to Emacs unless focused.
pub fn mouse_down<V: ViewClass>(this: &V, event: &NSEvent) {
    pointer_button(this, event, egui::PointerButton::Primary, true);
    let view: &NSView = this;
    if let Some(emacs) = emacs_view(view) {
        unsafe {
            let _: () = msg_send![&*emacs, mouseDown: event];
        }
    }
    if let Some(window) = view.window() {
        window.makeFirstResponder(Some(view));
    }
}

pub fn mouse_up<V: ViewClass>(this: &V, event: &NSEvent) {
    pointer_button(this, event, egui::PointerButton::Primary, false);
    if let Some(emacs) = emacs_view(this) {
        unsafe {
            let _: () = msg_send![&*emacs, mouseUp: event];
        }
    }
}

pub fn right_mouse_down<V: ViewClass>(this: &V, event: &NSEvent) {
    pointer_button(this, event, egui::PointerButton::Secondary, true);
}

pub fn right_mouse_up<V: ViewClass>(this: &V, event: &NSEvent) {
    pointer_button(this, event, egui::PointerButton::Secondary, false);
}

pub fn mouse_moved<V: ViewClass>(this: &V, event: &NSEvent) {
    let pos = local_pos(this, event);
    this.ivars()
        .shared
        .push_input(egui::Event::PointerMoved(pos));
}

pub fn mouse_exited<V: ViewClass>(this: &V) {
    this.ivars().shared.push_input(egui::Event::PointerGone);
}

/// The wheel scrolls the buffer, not the widget (contract, *Keyboard and
/// pointer*).
pub fn scroll_wheel<V: ViewClass>(this: &V, event: &NSEvent) {
    if let Some(emacs) = emacs_view(this) {
        unsafe {
            let _: () = msg_send![&*emacs, scrollWheel: event];
        }
    }
}

/// The keyboard policy. Unfocused: every key goes to Emacs. Focused: keys go
/// to egui, except `C-g`, which unfocuses and goes to Emacs.
///
/// The key is a real `keyDown:`, so Emacs is inside its event loop and
/// `EmacsView keyDown:` takes it as it is.
pub fn key<V: ViewClass>(this: &V, event: &NSEvent, down: bool) {
    let shared = this.ivars().shared.clone();
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
        set_focused(this, false);
        emit(shared.id, "unfocus", ":reason quit :via key");
    }
    if let Some(emacs) = emacs_view(this) {
        unsafe {
            let _: () = msg_send![&*emacs, keyDown: event];
        }
    }
}

/// AppKit offers a key with a modifier to the main menu before the first
/// responder, and Emacs's menu passes it straight to the `EmacsView`, so
/// `C-g` and every Control key would bypass a focused view. AppKit asks the
/// window's views first; a focused view that is first responder takes the key
/// here.
pub fn key_equivalent<V: ViewClass>(this: &V, event: &NSEvent) -> bool {
    if event.r#type() != NSEventType::KeyDown || !this.ivars().shared.focused.load(Ordering::SeqCst)
    {
        return false;
    }
    let view: &NSView = this;
    match view.window() {
        Some(window) if is_first_responder(view, &window) => {
            key(this, event, true);
            true
        }
        _ => false,
    }
}

pub(crate) fn set_focused<V: ViewClass>(this: &V, focused: bool) {
    let shared = &this.ivars().shared;
    if focused {
        // One focused view at a time.
        if let Some(previous) = focused_view::<V>() {
            if !ptr::eq(&*previous, this) {
                set_focused(&*previous, false);
            }
        }
        let me: Retained<NSView> = Retained::into_super(this.retain());
        FOCUSED.with(|f| *f.borrow_mut() = Some(Weak::from_retained(&me)));
        install_quit_monitor::<V>();
        shared.focus_frames.store(0, Ordering::SeqCst);
    } else if focused_view::<V>().is_some_and(|v| ptr::eq(&*v, this)) {
        FOCUSED.with(|f| *f.borrow_mut() = None);
    }
    shared.focused.store(focused, Ordering::SeqCst);
    shared.request_frame();
    let view: &NSView = this;
    let Some(window) = view.window() else {
        return;
    };
    if focused {
        window.makeFirstResponder(Some(view));
    } else if is_first_responder(view, &window) {
        let emacs = emacs_view(view);
        window.makeFirstResponder(emacs.as_deref().map(|v| -> &NSResponder { v }));
    }
}

/// Pause or resume the render thread when AppKit says the view was hidden,
/// shown, or moved to or out of a window. Hidden includes a hidden ancestor,
/// which is how Emacs hides a native xwidget's view.
pub fn update_visibility<V: ViewClass>(this: &V, by: &'static str) {
    let shared = &this.ivars().shared;
    let view: &NSView = this;
    let visible = view.window().is_some() && !view.isHiddenOrHasHiddenAncestor();
    if shared.visible.swap(visible, Ordering::SeqCst) == visible {
        return;
    }
    let now = std::time::Instant::now();
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
        shared.nudge();
    }
}

/// Before the view leaves its window, give the keys back to Emacs: a window
/// whose first responder is removed makes itself first responder, and the
/// Emacs frame would then take no key until clicked.
pub fn will_move_to_window<V: ViewClass>(this: &V, new_window: Option<&NSWindow>) {
    let view: &NSView = this;
    let Some(window) = view.window() else {
        return;
    };
    if new_window.is_some_and(|w| ptr::eq(w, &*window)) {
        return;
    }
    if is_first_responder(view, &window) {
        let emacs = emacs_view(view);
        window.makeFirstResponder(emacs.as_deref().map(|v| -> &NSResponder { v }));
    }
}

pub fn did_move_to_window<V: ViewClass>(this: &V) {
    let view: &NSView = this;
    if let Some(window) = view.window() {
        let scale = window.backingScaleFactor();
        lock(&this.ivars().shared.geometry).scale = scale as f32;
        if let Some(render) = this.ivars().render.borrow().as_ref() {
            render.layer.setContentsScale(scale);
        }
    }
    update_visibility(this, "viewDidMoveToWindow");
}

/// Emacs sizes the view (`nsxwidget_resize`); the Metal layer and egui's
/// screen follow. The class's `setFrameSize:` calls this after its super.
pub fn resized<V: ViewClass>(this: &V, size: NSSize) {
    let shared = &this.ivars().shared;
    {
        let mut geo = lock(&shared.geometry);
        geo.width = size.width as f32;
        geo.height = size.height as f32;
    }
    if let Some(render) = this.ivars().render.borrow().as_ref() {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        render
            .layer
            .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), size));
        CATransaction::commit();
    }
    shared.request_frame();
}

/// Stop the render thread and drop what it drew with. The view itself stays
/// wherever Emacs put it, showing its last frame.
pub(crate) fn stop_render<V: ViewClass>(this: &V) {
    let Some(render) = this.ivars().render.borrow_mut().take() else {
        return;
    };
    let shared = &this.ivars().shared;
    shared.stop();
    let _ = render.thread.join();
    render.layer.setDelegate(None);
    set_focused(this, false);
    emit(shared.id, "released", "");
}

thread_local! {
    /// The focused view, if any. Main thread only. Every view in this module
    /// is of the one class `export_app!` defined.
    static FOCUSED: RefCell<Option<Weak<NSView>>> = const { RefCell::new(None) };
    /// The monitor that unfocuses on `C-g`, installed with the first focus.
    static QUIT_MONITOR: RefCell<Option<Retained<AnyObject>>> = const { RefCell::new(None) };
}

fn focused_view<V: ViewClass>() -> Option<Retained<V>> {
    let view = FOCUSED.with(|f| f.borrow().as_ref().and_then(|w| w.load()))?;
    // SAFETY: only views of class V are stored in FOCUSED (`set_focused`).
    Some(unsafe { Retained::cast_unchecked::<V>(view) })
}

/// `C-g` must unfocus the focused view whichever way AppKit routes it. When
/// the application is not active, AppKit hands a Control key to the main menu
/// without asking the window's views, and Emacs's menu passes it to the
/// `EmacsView`; the view's own `keyDown:` and `performKeyEquivalent:` never
/// see it. A local monitor sees every key-down as it is dequeued, before any
/// of that; it unfocuses and lets the key go on to Emacs unchanged.
fn install_quit_monitor<V: ViewClass>() {
    QUIT_MONITOR.with(|m| {
        if m.borrow().is_some() {
            return;
        }
        let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
            let _ = panic::catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: AppKit passes a live event for the call.
                let ev = unsafe { event.as_ref() };
                if ev.r#type() == NSEventType::KeyDown && is_quit(ev) {
                    if let Some(view) = focused_view::<V>() {
                        set_focused(&*view, false);
                        emit(
                            view.ivars().shared.id,
                            "unfocus",
                            ":reason quit :via monitor",
                        );
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
pub(crate) fn letter_key_code(c: char) -> u16 {
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
