//! Native backend of emacs-egui: renders an [`EguiEmacsApp`] with Metal into
//! an `NSView` inside the Emacs process, from a dynamic module.
//!
//! An application crate builds as a `cdylib` and exports its module with one
//! macro:
//!
//! ```ignore
//! emacs_egui_native::export_app!(HudApp, "hud", HudApp::new);
//! ```
//!
//! That defines `plugin_is_GPL_compatible`, `emacs_module_init`, and, under
//! the prefix `emacs-egui-app-hud` (also the feature the module provides):
//!
//! | function | |
//! |---|---|
//! | `-open-channel PROC` | send every view's events to the pipe process PROC |
//! | `-make-view W H &optional THEME` | a view drawing a new instance of the application; returns its handle |
//! | `-release HANDLE` | stop the view and drop the module's reference |
//! | `-push HANDLE STATE` | hand STATE, a plist, to the application |
//! | `-push-theme HANDLE THEME` | hand THEME, a plist, to the view |
//! | `-focus HANDLE`, `-unfocus HANDLE` | let the view take keys, or give them back |
//! | `-id`, `-frames`, `-info` | the view's id, frame count and a plist about it |
//! | `-panic`, `-test-click`, `-test-mouse`, `-test-post-key`, `-test-magnify`, `-test-scroll`, `-test-counts`, `-test-deactivate` | test entries |
//!
//! The view is placed by Emacs, not by this module: the handle is a
//! `user-ptr` whose pointer is the view, ready for `(make-xwidget 'native
//! TITLE WIDTH HEIGHT HANDLE)`, and Emacs's redisplay moves, clips, resizes
//! and hides it as it does a WebKit xwidget. That needs the `native` xwidget
//! type, an Emacs patch (ebuild's `native-xwidget.patch`).
//!
//! Events arrive at Elisp as lines `(ID ACTION . PAYLOAD)` on the pipe
//! process: the module's own (`ready` after the first frame, `painted` after
//! a frame that showed a push or a theme, `hidden`, `shown`, `paused`,
//! `resumed`, `frames`, `unfocus`, `released`, `error`) and the
//! application's, from `emacs_post_message`.
//!
//! Threads: every module function runs on the Emacs main thread, which is
//! the AppKit main thread; each view has a render thread of its own
//! ([`render`]); one writer thread drains events into the pipe.

use std::sync::{Mutex, MutexGuard};

pub use emacs_egui_sdk as sdk;
use emacs_egui_sdk::EguiEmacsApp;
use objc2::{
    rc::Retained, runtime::ProtocolObject, ClassType, DefinedClass, MainThreadMarker,
    MainThreadOnly, Message,
};
use objc2_app_kit::NSView;
use objc2_foundation::NSRect;
use objc2_quartz_core::CALayerDelegate;

mod convert;
mod events;
mod lisp;
mod render;
#[doc(hidden)]
pub mod view;

pub use view::ViewIvars;

/// Lock M even if a thread panicked while holding it: every value behind these
/// locks stays valid between statements, and an AppKit callback must not panic.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One application module: what `export_app!` declares.
pub trait Module: 'static {
    type App: EguiEmacsApp + 'static;
    type View: ViewClass;
    /// The feature the module provides and the prefix of its functions.
    const PREFIX: &'static str;
    /// A new instance of the application, one per view.
    fn make_app() -> Self::App;
}

/// The view class `export_app!` defines in the application crate.
///
/// # Safety
///
/// Implemented by `export_app!` only: the class's methods call this crate's
/// view functions and its instance variables are [`ViewIvars`].
pub unsafe trait ViewClass:
    ClassType<Super = NSView>
    + DefinedClass<Ivars = ViewIvars>
    + MainThreadOnly
    + Message
    + std::ops::Deref<Target = NSView>
    + 'static
{
    fn create(mtm: MainThreadMarker, frame: NSRect, ivars: ViewIvars) -> Retained<Self>;
    fn layer_delegate() -> Retained<ProtocolObject<dyn CALayerDelegate>>;
}

pub(crate) mod panics {
    //! Panics: recorded with their location, and turned into Lisp errors at
    //! the module boundary or into an `error` event on a render thread.

    use std::{
        cell::RefCell,
        panic::{self, AssertUnwindSafe},
    };

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

    /// Record each panic's message and location, then run the previous hook.
    pub(crate) fn install_hook() {
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

    pub(crate) fn take_message(payload: &(dyn std::any::Any + Send)) -> String {
        LAST_PANIC
            .with(|p| p.borrow_mut().take())
            .unwrap_or_else(|| payload_message(payload))
    }

    /// Run a Lisp function's body. A panic in it goes on to the `emacs`
    /// crate's boundary, which signals `rust-panic`, but re-raised with a
    /// `String` payload: the crate reads the message only from a `String`,
    /// and a literal `panic!` or an `unwrap` on `None` carries a `&str`,
    /// which would reach Lisp as "Any { .. }".
    pub(crate) fn guard<T>(body: impl FnOnce() -> emacs::Result<T>) -> emacs::Result<T> {
        match panic::catch_unwind(AssertUnwindSafe(body)) {
            Ok(result) => result,
            Err(payload) => {
                let message = take_message(&*payload);
                panic::resume_unwind(Box::new(format!(
                    "Rust panic in emacs-egui-native: {message}"
                )))
            }
        }
    }
}

pub mod current {
    //! For an application that runs only on the native backend: the view it
    //! draws, from inside `update`, on that view's render thread. Called from
    //! any other thread, these do nothing.

    use crate::{lock, render};

    /// The view's numeric id, the first element of its event lines.
    pub fn id() -> Option<u64> {
        render::current_id()
    }

    /// Whether the view takes keys (`-focus`).
    pub fn focused() -> bool {
        render::current().is_some_and(|s| s.focused.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Whether the wheel and the trackpad's pinch reach egui. By default
    /// they scroll and zoom the Emacs buffer under the view (contract,
    /// *Keyboard and pointer*); a pan-and-zoom surface sets this once, from
    /// its first frame on.
    pub fn set_takes_wheel(takes: bool) {
        if let Some(s) = render::current() {
            s.takes_wheel
                .store(takes, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Show VALUE as `:KEY VALUE` in the view's `-info`.
    pub fn set_info(key: &str, value: serde_json::Value) {
        if let Some(s) = render::current() {
            let mut info = lock(&s.app_info);
            match info.iter_mut().find(|(k, _)| k == key) {
                Some(entry) => entry.1 = value,
                None => info.push((key.to_string(), value)),
            }
        }
    }

    /// A handle that asks the view for a frame from any thread. An
    /// application whose data arrives on its own thread (a socket reader, a
    /// timer) takes one in `update` and keeps it there, instead of polling
    /// with `request_repaint_after`. The handle does not keep the view alive:
    /// after the view is released, `request_repaint` does nothing.
    pub fn repaint_handle() -> Option<RepaintHandle> {
        render::current().map(|s| RepaintHandle(std::sync::Arc::downgrade(&s)))
    }

    /// See [`repaint_handle`].
    #[derive(Clone)]
    pub struct RepaintHandle(std::sync::Weak<render::Shared>);

    impl RepaintHandle {
        /// Ask the view for a frame; wakes its render thread if it sleeps.
        pub fn request_repaint(&self) {
            if let Some(s) = self.0.upgrade() {
                s.request_frame();
            }
        }
    }

    /// Name RECT, in the view's points, for `-test-click`.
    pub fn set_click_target(name: &str, rect: egui::Rect) {
        if let Some(s) = render::current() {
            let mut targets = lock(&s.click_targets);
            match targets.iter_mut().find(|(n, _)| n == name) {
                Some(entry) => entry.1 = rect,
                None => targets.push((name.to_string(), rect)),
            }
        }
    }
}

/// What `export_app!` expands to refers to, by these paths only.
#[doc(hidden)]
pub mod __rt {
    pub use crate::lisp::module_init;
    pub use crate::view;
    pub use emacs::raw::emacs_runtime;
    pub use objc2::{
        define_class, msg_send,
        rc::Retained,
        runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject},
        AllocAnyThread, MainThreadMarker, MainThreadOnly,
    };
    pub use objc2_app_kit::{NSEvent, NSResponder, NSView, NSWindow};
    pub use objc2_foundation::{NSRect, NSSize};
    pub use objc2_quartz_core::CALayerDelegate;
}

/// Export an application as an Emacs dynamic module.
///
/// - `export_app!(App, "name")`: functions and feature `emacs-egui-app-name`,
///   a new `App::default()` per view.
/// - `export_app!(App, "name", ctor)`: the same, a new `ctor()` per view.
/// - `export_app!(App, prefix = "my-prefix", ctor)`: a prefix of your own.
/// - `export_app!(App, prefix = "my-prefix", ctor, class = "MyView")`: also
///   the Objective-C name of the view class, for code that looks the view up
///   by name. The name must be unique in the process; a second module that
///   takes it panics when its first view is made.
///
/// Invoke it once, at the root of a `cdylib` crate. It also defines the
/// crate's view class and layer delegate class: Objective-C class names are
/// global to the process, so each application module defines classes of its
/// own, whose names objc2 derives from the crate's module path and version.
#[macro_export]
macro_rules! export_app {
    ($app:ty, $name:literal) => {
        $crate::export_app!(@module $app, ::core::concat!("emacs-egui-app-", $name),
                            <$app as ::core::default::Default>::default);
    };
    ($app:ty, $name:literal, $ctor:expr) => {
        $crate::export_app!(@module $app, ::core::concat!("emacs-egui-app-", $name), $ctor);
    };
    ($app:ty, prefix = $prefix:literal, $ctor:expr) => {
        $crate::export_app!(@module $app, $prefix, $ctor);
    };
    ($app:ty, prefix = $prefix:literal, $ctor:expr, class = $class:literal) => {
        $crate::export_app!(@module $app, $prefix, $ctor, $class);
    };
    (@module $app:ty, $prefix:expr, $ctor:expr $(, $class:literal)?) => {
        #[doc(hidden)]
        pub struct __EmacsEguiModule;

        impl $crate::Module for __EmacsEguiModule {
            type App = $app;
            type View = __EmacsEguiView;
            const PREFIX: &'static str = $prefix;
            fn make_app() -> $app {
                ($ctor)()
            }
        }

        /// This states that the module is GPL-compatible; Emacs loads no
        /// module without it.
        #[unsafe(no_mangle)]
        #[allow(non_upper_case_globals)]
        pub static plugin_is_GPL_compatible: ::core::ffi::c_int = 0;

        /// Entry point for Emacs's module loader.
        ///
        /// # Safety
        ///
        /// Called by Emacs, with its runtime.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn emacs_module_init(
            runtime: *mut $crate::__rt::emacs_runtime,
        ) -> ::core::ffi::c_int {
            unsafe { $crate::__rt::module_init::<__EmacsEguiModule>(runtime) }
        }

        // `define_class!` takes a protocol by its bare name.
        #[allow(unused_imports)]
        use $crate::__rt::{
            CALayerDelegate as __EmacsEguiCALayerDelegate,
            NSObjectProtocol as __EmacsEguiNSObjectProtocol,
        };

        $crate::__rt::define_class!(
            // SAFETY: NSObject has no subclassing requirements; no Drop impl.
            #[unsafe(super($crate::__rt::NSObject))]
            #[doc(hidden)]
            pub struct __EmacsEguiLayerDelegate;

            impl __EmacsEguiLayerDelegate {
                /// wgpu-hal walks up from the surface's layer to the first
                /// layer that has a delegate and asks it for its `window`, on
                /// every acquire, to skip occluded windows. That would be an
                /// AppKit call on the render thread; nil keeps it off AppKit.
                #[unsafe(method(window))]
                fn __window(&self) -> *mut $crate::__rt::AnyObject {
                    ::core::ptr::null_mut()
                }
            }

            unsafe impl __EmacsEguiNSObjectProtocol for __EmacsEguiLayerDelegate {}
            unsafe impl __EmacsEguiCALayerDelegate for __EmacsEguiLayerDelegate {}
        );

        $crate::__rt::define_class!(
            // SAFETY: NSView has no subclassing requirements; no Drop impl.
            #[unsafe(super($crate::__rt::NSView, $crate::__rt::NSResponder, $crate::__rt::NSObject))]
            #[thread_kind = $crate::__rt::MainThreadOnly]
            #[ivars = $crate::ViewIvars]
            $(#[name = $class])?
            #[doc(hidden)]
            pub struct __EmacsEguiView;

            impl __EmacsEguiView {
                #[unsafe(method(isFlipped))]
                fn __is_flipped(&self) -> bool {
                    true
                }

                #[unsafe(method(acceptsFirstResponder))]
                fn __accepts_first_responder(&self) -> bool {
                    true
                }

                #[unsafe(method(acceptsFirstMouse:))]
                fn __accepts_first_mouse(
                    &self,
                    _event: ::core::option::Option<&$crate::__rt::NSEvent>,
                ) -> bool {
                    true
                }

                #[unsafe(method(mouseDown:))]
                fn __mouse_down(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::mouse_down(self, event)
                }

                #[unsafe(method(mouseUp:))]
                fn __mouse_up(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::mouse_up(self, event)
                }

                #[unsafe(method(rightMouseDown:))]
                fn __right_mouse_down(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::right_mouse_down(self, event)
                }

                #[unsafe(method(rightMouseUp:))]
                fn __right_mouse_up(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::right_mouse_up(self, event)
                }

                #[unsafe(method(mouseMoved:))]
                fn __mouse_moved(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::mouse_moved(self, event)
                }

                #[unsafe(method(mouseDragged:))]
                fn __mouse_dragged(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::mouse_moved(self, event)
                }

                #[unsafe(method(mouseExited:))]
                fn __mouse_exited(&self, _event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::mouse_exited(self)
                }

                #[unsafe(method(scrollWheel:))]
                fn __scroll_wheel(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::scroll_wheel(self, event)
                }

                #[unsafe(method(magnifyWithEvent:))]
                fn __magnify_with_event(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::magnify(self, event)
                }

                #[unsafe(method(keyDown:))]
                fn __key_down(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::key(self, event, true)
                }

                #[unsafe(method(keyUp:))]
                fn __key_up(&self, event: &$crate::__rt::NSEvent) {
                    $crate::__rt::view::key(self, event, false)
                }

                #[unsafe(method(performKeyEquivalent:))]
                fn __perform_key_equivalent(&self, event: &$crate::__rt::NSEvent) -> bool {
                    $crate::__rt::view::key_equivalent(self, event)
                }

                /// As in nsxwidget: do nothing, so the input manager never
                /// turns a key into text behind the view's back.
                #[unsafe(method(interpretKeyEvents:))]
                fn __interpret_key_events(&self, _events: &$crate::__rt::AnyObject) {}

                #[unsafe(method(viewDidHide))]
                fn __view_did_hide(&self) {
                    $crate::__rt::view::update_visibility(self, "viewDidHide")
                }

                #[unsafe(method(viewDidUnhide))]
                fn __view_did_unhide(&self) {
                    $crate::__rt::view::update_visibility(self, "viewDidUnhide")
                }

                #[unsafe(method(viewWillMoveToWindow:))]
                fn __view_will_move_to_window(
                    &self,
                    new_window: ::core::option::Option<&$crate::__rt::NSWindow>,
                ) {
                    $crate::__rt::view::will_move_to_window(self, new_window)
                }

                #[unsafe(method(viewDidMoveToWindow))]
                fn __view_did_move_to_window(&self) {
                    $crate::__rt::view::did_move_to_window(self)
                }

                #[unsafe(method(setFrameSize:))]
                fn __set_frame_size(&self, size: $crate::__rt::NSSize) {
                    unsafe {
                        let _: () = $crate::__rt::msg_send![super(self), setFrameSize: size];
                    }
                    $crate::__rt::view::resized(self, size)
                }
            }

            unsafe impl __EmacsEguiNSObjectProtocol for __EmacsEguiView {}
        );

        unsafe impl $crate::ViewClass for __EmacsEguiView {
            fn create(
                mtm: $crate::__rt::MainThreadMarker,
                frame: $crate::__rt::NSRect,
                ivars: $crate::ViewIvars,
            ) -> $crate::__rt::Retained<Self> {
                let this = <Self as $crate::__rt::MainThreadOnly>::alloc(mtm).set_ivars(ivars);
                unsafe { $crate::__rt::msg_send![super(this), initWithFrame: frame] }
            }

            fn layer_delegate() -> $crate::__rt::Retained<
                $crate::__rt::ProtocolObject<dyn $crate::__rt::CALayerDelegate>,
            > {
                let this = <__EmacsEguiLayerDelegate as $crate::__rt::AllocAnyThread>::alloc()
                    .set_ivars(());
                let this: $crate::__rt::Retained<__EmacsEguiLayerDelegate> =
                    unsafe { $crate::__rt::msg_send![super(this), init] };
                $crate::__rt::ProtocolObject::from_retained(this)
            }
        }
    };
}
