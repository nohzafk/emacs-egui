//! The module's Lisp functions, defined for one application at module load.
//!
//! The `emacs` crate's `#[defun]` cannot define a function generic over the
//! application, so each function here is a plain Rust function, monomorphized
//! for the application's [`Module`], and `make_function` gets one trampoline
//! with that function's pointer as its data. The prefix is `Module::PREFIX`,
//! chosen by `export_app!`, so two application modules in one Emacs define
//! disjoint functions.
//!
//! A handle is a `user-ptr` whose pointer is the view, holding one reference
//! to it, so that `make-xwidget` can read the view straight out of it. Its
//! finalizer is `finalize_view`. `PREFIX-release` stops the view and drops
//! that reference at once, and then makes the pointer NULL, so the finalizer
//! of a released handle has nothing to do and `make-xwidget` refuses it.

use std::{
    ffi::{c_int, c_void},
    io::Write,
    ops::Range,
    panic, ptr,
    sync::{atomic::Ordering, mpsc, Arc},
    thread,
    time::Instant,
};

use dispatch2::DispatchQueue;
use emacs::{
    func::{CallEnv, HandleCall, Manage},
    raw::{emacs_env, emacs_runtime, emacs_value},
    Env, IntoLisp, Result, Value,
};
use emacs_egui_sdk::{AppHost, ThemeColors};
use objc2::{msg_send, rc::Retained, AnyThread, ClassType, DefinedClass, MainThreadMarker};
use objc2_app_kit::{
    NSApplication, NSEvent, NSEventModifierFlags, NSEventType, NSScreen, NSTrackingArea,
    NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSSize, NSString};
use objc2_quartz_core::{CALayer, CAMetalLayer};

use crate::{convert, events, lock, panics::guard, render, view, Module, ViewClass, ViewIvars};

type LispFn = for<'e> fn(&'e CallEnv) -> Result<Value<'e>>;

unsafe extern "C" fn trampoline(
    env: *mut emacs_env,
    nargs: isize,
    args: *mut emacs_value,
    data: *mut c_void,
) -> emacs_value {
    // SAFETY: `defun` stored a `LispFn` as this function's data.
    let f: LispFn = unsafe { std::mem::transmute::<*mut c_void, LispFn>(data) };
    let env = unsafe { CallEnv::new(Env::new(env), nargs, args) };
    env.handle_call(f)
}

fn defun(env: &Env, name: &str, f: LispFn, arity: Range<usize>, doc: &str) -> Result<()> {
    let func = unsafe { env.make_function(trampoline, arity, doc, f as *mut c_void) }?;
    env.fset(name, func)?;
    Ok(())
}

/// `emacs_module_init` for the application M; `export_app!` defines the
/// exported symbol and calls this.
///
/// # Safety
///
/// RUNTIME is the runtime Emacs passes to `emacs_module_init`.
pub unsafe fn module_init<M: Module>(runtime: *mut emacs_runtime) -> c_int {
    let env = unsafe { Env::from_runtime(runtime) };
    emacs::init::initialize(&env, |env| {
        crate::panics::install_hook();
        emacs_egui_sdk::set_message_sink(events::app_message);
        define_functions::<M>(env)?;
        env.provide(M::PREFIX)
    })
}

fn define_functions<M: Module>(env: &Env) -> Result<()> {
    let p = M::PREFIX;
    let defs: [(&str, LispFn, Range<usize>, &str); 15] = [
        (
            "open-channel",
            open_channel::<M>,
            1..1,
            "Route the events of every view of this module to PIPE-PROCESS.\n\n\
             PIPE-PROCESS is a process made by `make-pipe-process'.  Each event is one\n\
             line, (ID ACTION . PAYLOAD), where ID is the view's `-id'.\n\n\
             (fn PIPE-PROCESS)",
        ),
        (
            "make-view",
            make_view::<M>,
            2..3,
            "Make a view of WIDTH by HEIGHT points drawing a new instance of the application.\n\n\
             THEME, a plist (:bg :fg :font-size :font-family), is applied before the\n\
             first frame.  Return a handle for (make-xwidget \\='native TITLE WIDTH HEIGHT\n\
             HANDLE): a user-ptr whose pointer is the view.  The view draws once Emacs\n\
             shows it, and emits `ready' after its first frame.\n\n\
             (fn WIDTH HEIGHT &optional THEME)",
        ),
        (
            "release",
            release::<M>,
            1..1,
            "Stop HANDLE's view and drop the module's reference to it.\n\n\
             An xwidget that holds the view keeps its own reference until it is\n\
             killed.  Releasing a handle twice does nothing.\n\n(fn HANDLE)",
        ),
        (
            "push",
            push::<M>,
            2..2,
            "Hand STATE, a plist, to HANDLE's application on its next frame.\n\n\
             STATE is converted to the JSON value the web backend would parse, and\n\
             must deserialize into the application's state type; an error says where\n\
             it does not.  The view emits `painted' after the frame that shows it.\n\n\
             (fn HANDLE STATE)",
        ),
        (
            "push-theme",
            push_theme::<M>,
            2..2,
            "Hand THEME, a plist (:bg :fg :font-size :font-family), to HANDLE's view.\n\n\
             (fn HANDLE THEME)",
        ),
        (
            "focus",
            focus::<M>,
            1..1,
            "Let HANDLE's view take keys, all but C-g.\n\n(fn HANDLE)",
        ),
        (
            "unfocus",
            unfocus::<M>,
            1..1,
            "Give keys back to Emacs.\n\n(fn HANDLE)",
        ),
        (
            "frames",
            frames::<M>,
            1..1,
            "The number of frames HANDLE's view has rendered.\n\n(fn HANDLE)",
        ),
        (
            "id",
            id::<M>,
            1..1,
            "The numeric id of HANDLE's view, the first element of its event lines.\n\n\
             (fn HANDLE)",
        ),
        (
            "info",
            info::<M>,
            1..1,
            "A plist describing HANDLE's view, for tests and measures.\n\n(fn HANDLE)",
        ),
        (
            "panic",
            deliberate_panic::<M>,
            0..0,
            "Panic inside a module function, on purpose.",
        ),
        (
            "test-click",
            test_click::<M>,
            1..3,
            "Test entry: click the target the application named TARGET (default button).\n\n\
             Without DIRECT, a mouse-down and a mouse-up are posted to the\n\
             application's event queue; with DIRECT, they are handed to the view.\n\
             Return the point clicked, in window coordinates.\n\n\
             (fn HANDLE &optional DIRECT TARGET)",
        ),
        (
            "test-post-key",
            test_post_key::<M>,
            2..3,
            "Test entry: post a key-down and key-up for CHARS, one letter, to the\n\
             view's window; CTRL non-nil adds Control.\n\n(fn HANDLE CHARS &optional CTRL)",
        ),
        (
            "test-counts",
            test_counts::<M>,
            0..0,
            "Test entry: (:views N :threads N :in-windows N).",
        ),
        (
            "test-deactivate",
            test_deactivate::<M>,
            0..0,
            "Test entry: make the application inactive.",
        ),
    ];
    for (name, f, arity, doc) in defs {
        defun(env, &format!("{p}-{name}"), f, arity, doc)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

/// The finalizer of a view handle. Emacs runs finalizers during garbage
/// collection, which may happen on a Lisp thread; AppKit objects are released
/// on the main thread.
unsafe extern "C" fn finalize_view<V: ViewClass>(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let addr = p as usize;
    let release = move || {
        let _ = panic::catch_unwind(|| unsafe { release_view::<V>(addr as *mut c_void) });
    };
    if MainThreadMarker::new().is_some() {
        release();
    } else {
        DispatchQueue::main().exec_async(release);
    }
}

/// Stop the view P points to and drop the reference a handle held to it.
/// Main thread only.
unsafe fn release_view<V: ViewClass>(p: *mut c_void) {
    if let Some(view) = unsafe { Retained::from_raw(p as *mut V) } {
        view::stop_render(&*view);
    }
}

/// The view pointer of HANDLE, NULL once released; an error if HANDLE is not
/// a view handle of this module.
fn handle_pointer<M: Module>(handle: Value) -> Result<*mut c_void> {
    let fin = handle.get_user_finalizer()?;
    let ours: unsafe extern "C" fn(*mut c_void) = finalize_view::<M::View>;
    if !fin.is_some_and(|f| ptr::fn_addr_eq(f, ours)) {
        return Err(anyhow::anyhow!("not a view handle of {}", M::PREFIX).into());
    }
    handle.get_user_ptr()
}

fn view_of<M: Module>(handle: Value) -> Result<Retained<M::View>> {
    let p = handle_pointer::<M>(handle)? as *mut M::View;
    // SAFETY: a handle that is not released holds a reference to its view.
    unsafe { Retained::retain(p) }
        .ok_or_else(|| anyhow::anyhow!("the view handle was released").into())
}

fn main_thread() -> Result<MainThreadMarker> {
    MainThreadMarker::new().ok_or_else(|| {
        anyhow::anyhow!("emacs-egui-native must be called on the main thread").into()
    })
}

fn nil(env: &Env) -> Result<Value<'_>> {
    ().into_lisp(env)
}

fn number(env: &Env, v: Value) -> Result<f64> {
    if env.call("integerp", (v,))?.is_not_nil() {
        Ok(v.into_rust::<i64>()? as f64)
    } else {
        Ok(v.into_rust::<f64>()?)
    }
}

fn theme_of(env: &Env, v: Value) -> Result<ThemeColors> {
    let json = convert::lisp_to_json(env, v)?;
    serde_json::from_value(json).map_err(|e| anyhow::anyhow!("not a theme: {e}").into())
}

fn rect_list<'e>(env: &'e Env, r: NSRect) -> Result<Value<'e>> {
    env.list((r.origin.x, r.origin.y, r.size.width, r.size.height))
}

// ---------------------------------------------------------------------------
// Lisp functions
// ---------------------------------------------------------------------------

fn open_channel<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mut writer = env.open_channel(env.get_arg(0))?;
        let (tx, rx) = mpsc::channel::<String>();
        thread::Builder::new()
            .name(format!("{}-events", M::PREFIX))
            .spawn(move || {
                for line in rx {
                    if writer.write_all(line.as_bytes()).is_err() {
                        break;
                    }
                }
            })?;
        events::set_channel(tx);
        nil(env)
    })
}

fn make_view<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let created_at = Instant::now();
        let mtm = main_thread()?;
        let args = env.args();
        let (w, h) = (number(env, args[0])?, number(env, args[1])?);
        let theme = match args.get(2) {
            Some(t) if t.is_not_nil() => Some(theme_of(env, *t)?),
            _ => None,
        };
        let scale = NSScreen::mainScreen(mtm)
            .map(|s| s.backingScaleFactor())
            .unwrap_or(2.0);
        let gpu = render::gpu()?;

        let id = render::NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let shared = Arc::new(render::Shared::new(id, w, h, scale, created_at));
        if let Some(theme) = theme {
            shared.push_theme(theme);
        }
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
        let view = M::View::create(mtm, frame, ViewIvars::new(shared.clone()));
        let nsview: &NSView = &view;

        // Layer-hosting view: a plain root layer, and the Metal layer under it
        // with a delegate whose `window` is nil, so wgpu-hal never walks to
        // AppKit from the render thread; sized by `view::resized`.
        let host = CALayer::new();
        nsview.setLayer(Some(&host));
        nsview.setWantsLayer(true);
        let layer = CAMetalLayer::new();
        layer.setFrame(frame);
        layer.setContentsScale(scale);
        layer.setOpaque(true);
        let delegate = M::View::layer_delegate();
        layer.setDelegate(Some(&*delegate));
        host.addSublayer(&layer);

        let tracking = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                frame,
                NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(nsview),
                None,
            )
        };
        nsview.addTrackingArea(&tracking);

        let surface = render::surface_on(&gpu, &layer)?;
        let thread = render::spawn(
            shared.clone(),
            gpu,
            surface,
            render::SendLayer(layer.clone()),
            M::make_app,
        )?;
        *view.ivars().render.borrow_mut() = Some(view::Render {
            layer,
            _delegate: delegate,
            thread,
        });

        let p = Retained::into_raw(view) as *mut c_void;
        unsafe { env.make_user_ptr(Some(finalize_view::<M::View>), p) }
    })
}

fn release<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        main_thread()?;
        let p = handle_pointer::<M>(env.get_arg(0))?;
        if p.is_null() {
            return nil(env);
        }
        // The `emacs` crate has no `set_user_ptr`; the raw call clears it.
        unsafe {
            let raw = env.raw();
            let set = (*raw).set_user_ptr.expect("emacs_env.set_user_ptr");
            set(raw, env.raw_args()[0], ptr::null_mut());
        }
        unsafe { release_view::<M::View>(p) };
        nil(env)
    })
}

fn push<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let view = view_of::<M>(env.get_arg(0))?;
        let value = convert::lisp_to_json(env, env.get_arg(1))?;
        // Deserialize here too, so a state that does not fit is an error now
        // and not a lost frame later.
        if let Err(e) = AppHost::<M::App>::decode(value.clone()) {
            return Err(anyhow::anyhow!("the state does not fit the application: {e}").into());
        }
        view.ivars().shared.push_state(value);
        nil(env)
    })
}

fn push_theme<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let view = view_of::<M>(env.get_arg(0))?;
        let theme = theme_of(env, env.get_arg(1))?;
        view.ivars().shared.push_theme(theme);
        nil(env)
    })
}

fn focus<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        main_thread()?;
        view::set_focused(&*view_of::<M>(env.get_arg(0))?, true);
        nil(env)
    })
}

fn unfocus<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        main_thread()?;
        view::set_focused(&*view_of::<M>(env.get_arg(0))?, false);
        nil(env)
    })
}

fn frames<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let n = view_of::<M>(env.get_arg(0))?
            .ivars()
            .shared
            .frames
            .load(Ordering::SeqCst) as i64;
        n.into_lisp(env)
    })
}

fn id<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| (view_of::<M>(env.get_arg(0))?.ivars().shared.id as i64).into_lisp(env))
}

/// Geometry is in points: `:frame` in its container, `:in-emacs-view` in the
/// `EmacsView`, `:visible-rect` the part AppKit reports unclipped by
/// ancestors. What the application published with `current::set_info`
/// follows the module's own keys.
fn info<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        let view = view_of::<M>(env.get_arg(0))?;
        let nsview: &NSView = &view;
        let s = &view.ivars().shared;
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
            None => (nil(env)?, nil(env)?),
        };
        let emacs = view::emacs_view(nsview);
        let in_emacs = match emacs.as_deref() {
            Some(e) => rect_list(env, nsview.convertRect_toView(nsview.bounds(), Some(e)))?,
            None => nil(env)?,
        };
        // SAFETY: as in `view::emacs_view`.
        let container = unsafe { nsview.superview().and_then(|c| c.superview()) };
        let (container_frame, container_clips) = match container.as_deref() {
            Some(c) => (
                rect_list(env, c.frame())?,
                c.clipsToBounds().into_lisp(env)?,
            ),
            None => (nil(env)?, nil(env)?),
        };
        let window = nsview.window();
        let first_responder = match window.as_deref().and_then(|w| w.firstResponder()) {
            Some(r) => r
                .class()
                .name()
                .to_string_lossy()
                .into_owned()
                .into_lisp(env)?,
            None => nil(env)?,
        };
        let key_window = NSApplication::sharedApplication(mtm)
            .keyWindow()
            .zip(window.as_deref())
            .is_some_and(|(k, w)| ptr::eq(&*k, w));
        let (pushes, themes) = s.push_counts();
        let mut vals: Vec<Value> = vec![
            env.intern(":id")?,
            (s.id as i64).into_lisp(env)?,
            env.intern(":prefix")?,
            M::PREFIX.into_lisp(env)?,
            env.intern(":class")?,
            M::View::class()
                .name()
                .to_string_lossy()
                .into_owned()
                .into_lisp(env)?,
            env.intern(":rendering")?,
            render.is_some().into_lisp(env)?,
            env.intern(":visible")?,
            s.visible.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":frames")?,
            (s.frames.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":fps")?,
            (*lock(&s.fps)).into_lisp(env)?,
            env.intern(":pushes")?,
            (pushes as i64).into_lisp(env)?,
            env.intern(":themes")?,
            (themes as i64).into_lisp(env)?,
            env.intern(":hides")?,
            (s.hides.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":unhides")?,
            (s.unhides.load(Ordering::SeqCst) as i64).into_lisp(env)?,
            env.intern(":pause-ms")?,
            (*lock(&s.pause_ms)).into_lisp(env)?,
            env.intern(":resume-ms")?,
            (*lock(&s.resume_ms)).into_lisp(env)?,
            env.intern(":focused")?,
            s.focused.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":egui-focus")?,
            s.egui_focus.load(Ordering::SeqCst).into_lisp(env)?,
            env.intern(":first-frame-ms")?,
            (*lock(&s.first_frame_ms)).into_lisp(env)?,
            env.intern(":scale")?,
            (geo.scale as f64).into_lisp(env)?,
            env.intern(":format")?,
            lock(&s.surface_format).clone().into_lisp(env)?,
            env.intern(":frame")?,
            rect_list(env, nsview.frame())?,
            env.intern(":in-emacs-view")?,
            in_emacs,
            env.intern(":visible-rect")?,
            rect_list(env, nsview.visibleRect())?,
            env.intern(":hidden-or-ancestor")?,
            nsview.isHiddenOrHasHiddenAncestor().into_lisp(env)?,
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
        for (k, v) in lock(&s.app_info).iter() {
            vals.push(env.intern(&format!(":{k}"))?);
            vals.push(convert::json_to_lisp(env, v)?);
        }
        env.call("list", &vals[..])
    })
}

fn deliberate_panic<M: Module>(_env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        panic!(
            "{}-panic: deliberate panic inside a module function",
            M::PREFIX
        );
    })
}

fn test_click<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        let args = env.args();
        let view = view_of::<M>(args[0])?;
        let nsview: &NSView = &view;
        let direct = args.get(1).is_some_and(|d| d.is_not_nil());
        let target = match args.get(2) {
            Some(t) if t.is_not_nil() => env.call("symbol-name", (*t,))?.into_rust::<String>()?,
            _ => "button".to_string(),
        };
        let rect = lock(&view.ivars().shared.click_targets)
            .iter()
            .find(|(name, _)| *name == target)
            .map(|(_, r)| *r)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no click target `{target}': the application names them with \
                     current::set_click_target, after its first frame"
                )
            })?;
        let center = rect.center();
        let window = nsview
            .window()
            .ok_or_else(|| anyhow::anyhow!("view has no window"))?;
        let p = nsview.convertPoint_toView(NSPoint::new(center.x as f64, center.y as f64), None);
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
        if direct {
            unsafe {
                let _: () = msg_send![nsview, mouseDown: &*down];
                let _: () = msg_send![nsview, mouseUp: &*up];
            }
        } else {
            let app = NSApplication::sharedApplication(mtm);
            app.postEvent_atStart(&down, false);
            app.postEvent_atStart(&up, false);
        }
        env.list((p.x, p.y, if direct { "direct" } else { "posted" }))
    })
}

fn test_post_key<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        let args = env.args();
        let view = view_of::<M>(args[0])?;
        let chars = args[1].into_rust::<String>()?;
        let ctrl = args.get(2).is_some_and(|v| v.is_not_nil());
        let nsview: &NSView = &view;
        let window = nsview
            .window()
            .ok_or_else(|| anyhow::anyhow!("view has no window"))?;
        let c = chars
            .chars()
            .next()
            .ok_or_else(|| anyhow::anyhow!("CHARS is empty"))?;
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
                view::letter_key_code(c),
            )
            .ok_or_else(|| anyhow::anyhow!("cannot make a key event"))?;
            app.postEvent_atStart(&event, false);
        }
        nil(env)
    })
}

/// `(:views N :threads N :in-windows N)`: the views not yet deallocated, the
/// render threads still running, and the views of this module found in the
/// view hierarchy of any of the application's windows.
fn test_counts<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        fn count<V: ViewClass>(view: &NSView) -> i64 {
            let here = i64::from(view.isKindOfClass(V::class()));
            here + view.subviews().iter().map(|v| count::<V>(&v)).sum::<i64>()
        }
        let in_windows: i64 = NSApplication::sharedApplication(mtm)
            .windows()
            .iter()
            .filter_map(|w| w.contentView())
            .map(|v| count::<M::View>(&v))
            .sum();
        env.list((
            env.intern(":views")?,
            render::LIVE_VIEWS.load(Ordering::SeqCst),
            env.intern(":threads")?,
            render::LIVE_THREADS.load(Ordering::SeqCst),
            env.intern(":in-windows")?,
            in_windows,
        ))
    })
}

fn test_deactivate<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        NSApplication::sharedApplication(mtm).deactivate();
        nil(env)
    })
}
