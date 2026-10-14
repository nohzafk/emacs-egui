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
use objc2_quartz_core::CALayer;

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
    let defs: [(&str, LispFn, Range<usize>, &str); 19] = [
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
            "test-mouse",
            test_mouse::<M>,
            4..5,
            "Test entry: post one left-button event of KIND (`down', `drag' or `up')\n\
             at X, Y in HANDLE's view, in points.  Without VIA-PID, to the\n\
             application's event queue, addressed to the view's window; with\n\
             VIA-PID, through the window server to this process (CGEventPostToPid),\n\
             which routes it by screen location as it does a real mouse's, without\n\
             moving the pointer.  Return the point in window coordinates.\n\n\
             (fn HANDLE KIND X Y &optional VIA-PID)",
        ),
        (
            "test-magnify",
            test_magnify::<M>,
            2..3,
            "Test entry: a trackpad pinch of MAGNIFICATION (0.1 is a 10% spread) at\n\
             the centre of HANDLE's view.  Without DIRECT, the event is posted to the\n\
             application's event queue and AppKit routes it; with DIRECT, it is\n\
             handed to the view's `magnifyWithEvent:'.  Return (TYPE MAGNIFICATION\n\
             WINDOW-NUMBER) of the event made.\n\n\
             (fn HANDLE MAGNIFICATION &optional DIRECT)",
        ),
        (
            "test-scroll",
            test_scroll::<M>,
            2..3,
            "Test entry: a continuous (trackpad) scroll of DY pixels at the centre of\n\
             HANDLE's view, posted or, with DIRECT, handed to `scrollWheel:'.  Return\n\
             (TYPE DELTA-Y WINDOW-NUMBER) of the event made.\n\n\
             (fn HANDLE DY &optional DIRECT)",
        ),
        (
            "test-watch-mouse",
            test_watch_mouse::<M>,
            0..0,
            "Test entry: count the left-button events the application dequeues.\n\n\
             The first call installs a local event monitor; every call returns\n\
             (:downs N :drags N :ups N :last-window N :last-hit CLASS), where\n\
             LAST-HIT names the class of the view the event's window hit-tests\n\
             at the last event's location, the view AppKit would send it to.",
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
        let delegate = M::View::layer_delegate();
        let layer = view::metal_layer(frame, scale, &delegate);
        host.addSublayer(&layer);
        shared.set_layer(layer.clone());

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
            shared.layers.load(Ordering::SeqCst),
            M::make_app,
        )?;
        *view.ivars().render.borrow_mut() = Some(view::Render {
            layer,
            delegate,
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
        let (layer, drawable, max_drawables) = match render.as_ref() {
            Some(r) => {
                let b = r.layer.bounds();
                let d = r.layer.drawableSize();
                (
                    env.list((b.size.width, b.size.height))?,
                    env.list((d.width, d.height))?,
                    (r.layer.maximumDrawableCount() as i64).into_lisp(env)?,
                )
            }
            None => (nil(env)?, nil(env)?, nil(env)?),
        };
        let (frame_latency, present_mode) = match lock(&s.surface_config).clone() {
            Some((latency, mode)) => ((latency as i64).into_lisp(env)?, mode.into_lisp(env)?),
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
            env.intern(":release-ms")?,
            (*lock(&s.release_ms)).into_lisp(env)?,
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
            env.intern(":max-drawables")?,
            max_drawables,
            env.intern(":frame-latency")?,
            frame_latency,
            env.intern(":present-mode")?,
            present_mode,
            env.intern(":egui-screen")?,
            env.list((egui_w as f64, egui_h as f64))?,
            env.intern(":first-responder")?,
            first_responder,
            env.intern(":mouse")?,
            env.list((
                s.mouse[0].load(Ordering::SeqCst) as i64,
                s.mouse[1].load(Ordering::SeqCst) as i64,
                s.mouse[2].load(Ordering::SeqCst) as i64,
                s.mouse[3].load(Ordering::SeqCst) as i64,
            ))?,
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

fn test_mouse<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        let args = env.args();
        let view = view_of::<M>(args[0])?;
        let kind = env.call("symbol-name", (args[1],))?.into_rust::<String>()?;
        let (x, y) = (number(env, args[2])?, number(env, args[3])?);
        let nsview: &NSView = &view;
        let window = nsview
            .window()
            .ok_or_else(|| anyhow::anyhow!("view has no window"))?;
        let (kind, pressure, cg_kind) = match kind.as_str() {
            "down" => (NSEventType::LeftMouseDown, 1.0, 1),
            "drag" => (NSEventType::LeftMouseDragged, 1.0, 6),
            "up" => (NSEventType::LeftMouseUp, 0.0, 2),
            other => return Err(anyhow::anyhow!("KIND is down, drag or up, not {other}").into()),
        };
        let p = nsview.convertPoint_toView(NSPoint::new(x, y), None);
        if args.get(4).is_some_and(|v| v.is_not_nil()) {
            let on_screen = window.convertPointToScreen(p);
            let main_height = NSScreen::screens(mtm)
                .iter()
                .next()
                .map(|s| s.frame().size.height)
                .unwrap_or(0.0);
            let at = CGPoint {
                x: on_screen.x,
                y: main_height - on_screen.y,
            };
            unsafe {
                // kCGMouseButtonLeft
                let cg = CGEventCreateMouseEvent(ptr::null(), cg_kind, at, 0);
                if cg.is_null() {
                    return Err(anyhow::anyhow!("CoreGraphics made no event").into());
                }
                CGEventPostToPid(std::process::id() as i32, cg);
                CFRelease(cg);
            }
            return env.list((p.x, p.y));
        }
        let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            kind,
            p,
            NSEventModifierFlags::empty(),
            NSProcessInfo::processInfo().systemUptime(),
            window.windowNumber(),
            None,
            0,
            1,
            pressure,
        )
        .ok_or_else(|| anyhow::anyhow!("cannot make a mouse event"))?;
        NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false);
        env.list((p.x, p.y))
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

// Synthetic gesture and scroll events. AppKit has no public constructor for
// a magnify event; one made by CoreGraphics as a gesture event of HID type
// zoom becomes an `NSEventTypeMagnify` NSEvent.

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        kind: u32,
        location: CGPoint,
        button: u32,
    ) -> *mut c_void;
    fn CGEventPostToPid(pid: i32, event: *mut c_void);
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut c_void;
    fn CGEventSetType(event: *mut c_void, kind: u32);
    fn CGEventSetLocation(event: *mut c_void, location: CGPoint);
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: *mut c_void, field: u32, value: f64);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// CGEvent fields AppKit reads for gestures and for the target window, from
/// the window server's private event layout.
const CG_GESTURE: u32 = 29;
const FIELD_GESTURE_HID_TYPE: u32 = 110;
const FIELD_GESTURE_ZOOM_VALUE: u32 = 113;
const FIELD_GESTURE_PHASE: u32 = 132;
const HID_TYPE_ZOOM: i64 = 8;
const FIELD_WINDOW_ID: u32 = 51;
const FIELD_WINDOW_UNDER_POINTER: u32 = 91;
const FIELD_WINDOW_UNDER_POINTER_THAT_CAN_HANDLE: u32 = 92;
const FIELD_SCROLL_IS_CONTINUOUS: u32 = 88;

/// The centre of VIEW in CoreGraphics global coordinates (origin at the top
/// left of the main display), and its window's number.
fn gesture_target(mtm: MainThreadMarker, view: &NSView) -> Result<(CGPoint, isize)> {
    let window = view
        .window()
        .ok_or_else(|| anyhow::anyhow!("view has no window"))?;
    let b = view.bounds();
    let centre = NSPoint::new(
        b.origin.x + b.size.width / 2.0,
        b.origin.y + b.size.height / 2.0,
    );
    let in_window = view.convertPoint_toView(centre, None);
    let on_screen = window.convertPointToScreen(in_window);
    let main_height = NSScreen::screens(mtm)
        .iter()
        .next()
        .map(|s| s.frame().size.height)
        .unwrap_or(0.0);
    Ok((
        CGPoint {
            x: on_screen.x,
            y: main_height - on_screen.y,
        },
        window.windowNumber(),
    ))
}

/// Turn the CGEvent CG (consumed) into an NSEvent and deliver it to VIEW
/// with SELECTOR when DIRECT, or post it to the application's queue.
fn deliver_cg_event(
    mtm: MainThreadMarker,
    view: &NSView,
    cg: *mut c_void,
    window_number: isize,
    direct: bool,
    magnify: bool,
) -> Result<Retained<NSEvent>> {
    if cg.is_null() {
        return Err(anyhow::anyhow!("CoreGraphics made no event").into());
    }
    unsafe {
        CGEventSetIntegerValueField(cg, FIELD_WINDOW_ID, window_number as i64);
        CGEventSetIntegerValueField(cg, FIELD_WINDOW_UNDER_POINTER, window_number as i64);
        CGEventSetIntegerValueField(
            cg,
            FIELD_WINDOW_UNDER_POINTER_THAT_CAN_HANDLE,
            window_number as i64,
        );
    }
    // The NSEvent made below has no location in its window; move the
    // pointer to the view's centre first, as the tracking area does when a
    // real pointer arrives.
    let b = view.bounds();
    let centre = view.convertPoint_toView(
        NSPoint::new(
            b.origin.x + b.size.width / 2.0,
            b.origin.y + b.size.height / 2.0,
        ),
        None,
    );
    let moved = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::MouseMoved,
        centre,
        NSEventModifierFlags::empty(),
        NSProcessInfo::processInfo().systemUptime(),
        window_number,
        None,
        0,
        0,
        0.0,
    )
    .ok_or_else(|| anyhow::anyhow!("cannot make a mouse event"))?;
    unsafe {
        let _: () = msg_send![view, mouseMoved: &*moved];
    }
    let event: Option<Retained<NSEvent>> =
        unsafe { msg_send![NSEvent::class(), eventWithCGEvent: cg] };
    unsafe { CFRelease(cg) };
    let event = event.ok_or_else(|| anyhow::anyhow!("AppKit made no NSEvent of it"))?;
    if direct {
        unsafe {
            if magnify {
                let _: () = msg_send![view, magnifyWithEvent: &*event];
            } else {
                let _: () = msg_send![view, scrollWheel: &*event];
            }
        }
    } else {
        NSApplication::sharedApplication(mtm).postEvent_atStart(&event, false);
    }
    Ok(event)
}

fn test_magnify<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        let args = env.args();
        let view = view_of::<M>(args[0])?;
        let magnification = number(env, args[1])?;
        let direct = args.get(2).is_some_and(|d| d.is_not_nil());
        let nsview: &NSView = &view;
        let (at, window_number) = gesture_target(mtm, nsview)?;
        let cg = unsafe {
            let cg = CGEventCreate(ptr::null());
            if !cg.is_null() {
                CGEventSetType(cg, CG_GESTURE);
                CGEventSetLocation(cg, at);
                CGEventSetIntegerValueField(cg, FIELD_GESTURE_HID_TYPE, HID_TYPE_ZOOM);
                CGEventSetDoubleValueField(cg, FIELD_GESTURE_ZOOM_VALUE, magnification);
                // NSEventPhaseChanged
                CGEventSetIntegerValueField(cg, FIELD_GESTURE_PHASE, 4);
            }
            cg
        };
        let event = deliver_cg_event(mtm, nsview, cg, window_number, direct, true)?;
        let kind = event.r#type().0 as i64;
        let made = if event.r#type() == NSEventType::Magnify {
            event.magnification()
        } else {
            0.0
        };
        env.list((kind, made, event.windowNumber() as i64))
    })
}

fn test_scroll<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let mtm = main_thread()?;
        let args = env.args();
        let view = view_of::<M>(args[0])?;
        let dy = number(env, args[1])?;
        let direct = args.get(2).is_some_and(|d| d.is_not_nil());
        let nsview: &NSView = &view;
        let (at, window_number) = gesture_target(mtm, nsview)?;
        let cg = unsafe {
            // kCGScrollEventUnitPixel
            let cg = CGEventCreateScrollWheelEvent2(ptr::null(), 0, 1, dy.round() as i32, 0, 0);
            if !cg.is_null() {
                CGEventSetLocation(cg, at);
                CGEventSetIntegerValueField(cg, FIELD_SCROLL_IS_CONTINUOUS, 1);
            }
            cg
        };
        let event = deliver_cg_event(mtm, nsview, cg, window_number, direct, false)?;
        env.list((
            event.r#type().0 as i64,
            event.scrollingDeltaY(),
            event.windowNumber() as i64,
        ))
    })
}

thread_local! {
    /// What `-test-watch-mouse`'s monitor counted: downs, drags, ups, the last
    /// event's window number and the class of the view it hit. Main thread.
    static MOUSE_WATCH: std::cell::RefCell<Option<(Retained<objc2::runtime::AnyObject>, [i64; 4], String)>> =
        const { std::cell::RefCell::new(None) };
}

fn test_watch_mouse<M: Module>(env: &CallEnv) -> Result<Value<'_>> {
    guard(|| {
        let _ = main_thread()?;
        MOUSE_WATCH.with(|w| {
            if w.borrow().is_some() {
                return;
            }
            let block = block2::RcBlock::new(|event: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
                let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                    // SAFETY: AppKit passes a live event for the call.
                    let ev = unsafe { event.as_ref() };
                    let slot = match ev.r#type() {
                        NSEventType::LeftMouseDown => 0,
                        NSEventType::LeftMouseDragged => 1,
                        NSEventType::LeftMouseUp => 2,
                        _ => return,
                    };
                    let hit = ev
                        .window(MainThreadMarker::new().expect("a monitor runs on the main thread"))
                        .and_then(|win| {
                            let content = win.contentView()?;
                            let sup = unsafe { content.superview() }.unwrap_or(content);
                            let p = sup.convertPoint_fromView(ev.locationInWindow(), None);
                            sup.hitTest(p)
                        })
                        .map(|v| v.class().name().to_string_lossy().into_owned())
                        .unwrap_or_else(|| "none".to_string());
                    MOUSE_WATCH.with(|w| {
                        if let Some((_, counts, last)) = w.borrow_mut().as_mut() {
                            counts[slot] += 1;
                            counts[3] = ev.windowNumber() as i64;
                            *last = hit;
                        }
                    });
                }));
                event.as_ptr()
            });
            let mask = objc2_app_kit::NSEventMask::LeftMouseDown
                | objc2_app_kit::NSEventMask::LeftMouseDragged
                | objc2_app_kit::NSEventMask::LeftMouseUp;
            if let Some(monitor) =
                unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }
            {
                *w.borrow_mut() = Some((monitor, [0; 4], String::new()));
            }
        });
        let (counts, last) = MOUSE_WATCH.with(|w| {
            w.borrow()
                .as_ref()
                .map(|(_, c, l)| (*c, l.clone()))
                .unwrap_or_default()
        });
        env.list((
            env.intern(":downs")?,
            counts[0],
            env.intern(":drags")?,
            counts[1],
            env.intern(":ups")?,
            counts[2],
            env.intern(":last-window")?,
            counts[3],
            env.intern(":last-hit")?,
            last,
        ))
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
