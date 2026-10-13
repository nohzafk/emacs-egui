//! The GPU every view shares, the state a view's main-thread side and its
//! render thread share, and the render thread itself.
//!
//! Each view has a render thread of its own. It owns the egui context, the
//! application and the wgpu surface, and draws into a `CAMetalLayer` without
//! the main thread, so it keeps drawing while Elisp is busy. It draws only
//! when there is something to draw: input, a push, a resize, an unhide, or a
//! repaint egui asked for. Otherwise it sleeps, and while the view is hidden
//! it sleeps whatever egui asked.

use std::{
    cell::RefCell,
    ffi::c_void,
    panic::{self, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use egui_wgpu::wgpu;
use emacs_egui_sdk::{parse_hex_color, AppHost, Applied, EguiEmacsApp, ThemeColors};
use objc2::rc::Retained;
use objc2_quartz_core::CAMetalLayer;

use crate::{convert::lisp_string, events::emit, lock, panics};

// ---------------------------------------------------------------------------
// GPU, shared by every view
// ---------------------------------------------------------------------------

pub(crate) struct Gpu {
    pub(crate) instance: wgpu::Instance,
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

pub(crate) fn gpu() -> anyhow::Result<Arc<Gpu>> {
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
pub(crate) struct Geometry {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) scale: f32,
}

/// Why the render thread should draw next.
#[derive(Default)]
pub(crate) struct Wake {
    /// Something changed: input, a push, a resize, focus, an unhide.
    dirty: bool,
    /// When egui asked to be drawn again, if it did.
    repaint_at: Option<Instant>,
}

/// What Emacs pushed and the render thread has not drawn yet.
#[derive(Default)]
struct Pushed {
    state: Option<serde_json::Value>,
    theme: Option<ThemeColors>,
    /// How many states and themes Emacs has pushed, the undrawn ones included.
    states: u64,
    themes: u64,
    /// When the oldest undrawn push arrived.
    since: Option<Instant>,
}

pub(crate) struct Shared {
    pub(crate) id: u64,
    pub(crate) running: AtomicBool,
    pub(crate) focused: AtomicBool,
    /// In a window and neither hidden nor under a hidden ancestor.
    pub(crate) visible: AtomicBool,
    wake: Mutex<Wake>,
    wake_cv: Condvar,
    pub(crate) frames: AtomicU64,
    pub(crate) hides: AtomicU64,
    pub(crate) unhides: AtomicU64,
    input: Mutex<Vec<egui::Event>>,
    pushed: Mutex<Pushed>,
    pub(crate) geometry: Mutex<Geometry>,
    /// The screen rectangle egui laid out the last frame in, in points.
    pub(crate) egui_screen: Mutex<(f32, f32)>,
    created_at: Instant,
    pub(crate) first_frame_ms: Mutex<Option<f64>>,
    pub(crate) fps: Mutex<f64>,
    pub(crate) surface_format: Mutex<String>,
    /// Whether a widget held egui's keyboard focus in the last frame.
    pub(crate) egui_focus: AtomicBool,
    /// Frames drawn since the view was last focused.
    pub(crate) focus_frames: AtomicU64,
    /// When the view was hidden, until the render thread has paused.
    pub(crate) hidden_at: Mutex<Option<Instant>>,
    /// When and by which callback the view became visible, until the render
    /// thread has drawn after it.
    pub(crate) shown_at: Mutex<Option<(Instant, &'static str)>>,
    pub(crate) pause_ms: Mutex<Option<f64>>,
    pub(crate) resume_ms: Mutex<Option<f64>>,
    /// What the application published for `-info` (`current::set_info`).
    pub(crate) app_info: Mutex<Vec<(String, serde_json::Value)>>,
    /// Rectangles the application named for `-test-click`.
    pub(crate) click_targets: Mutex<Vec<(String, egui::Rect)>>,
}

impl Shared {
    pub(crate) fn new(id: u64, width: f64, height: f64, scale: f64, created_at: Instant) -> Self {
        Self {
            id,
            running: AtomicBool::new(true),
            focused: AtomicBool::new(false),
            visible: AtomicBool::new(false),
            wake: Mutex::new(Wake {
                dirty: true,
                repaint_at: None,
            }),
            wake_cv: Condvar::new(),
            frames: AtomicU64::new(0),
            hides: AtomicU64::new(0),
            unhides: AtomicU64::new(0),
            input: Mutex::new(Vec::new()),
            pushed: Mutex::new(Pushed::default()),
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
            egui_focus: AtomicBool::new(false),
            focus_frames: AtomicU64::new(0),
            hidden_at: Mutex::new(None),
            shown_at: Mutex::new(None),
            pause_ms: Mutex::new(None),
            resume_ms: Mutex::new(None),
            app_info: Mutex::new(Vec::new()),
            click_targets: Mutex::new(Vec::new()),
        }
    }

    /// Ask the render thread for a frame.
    pub(crate) fn request_frame(&self) {
        lock(&self.wake).dirty = true;
        self.wake_cv.notify_all();
    }

    /// Wake the render thread without asking for a frame, so it notices a
    /// change in visibility or a stop.
    pub(crate) fn nudge(&self) {
        let _wake = lock(&self.wake);
        self.wake_cv.notify_all();
    }

    pub(crate) fn push_input(&self, event: egui::Event) {
        lock(&self.input).push(event);
        self.request_frame();
    }

    /// Replace the state the next frame hands the application; a state not
    /// drawn yet is superseded.
    pub(crate) fn push_state(&self, state: serde_json::Value) {
        {
            let mut p = lock(&self.pushed);
            p.state = Some(state);
            p.states += 1;
            p.since.get_or_insert_with(Instant::now);
        }
        self.request_frame();
    }

    pub(crate) fn push_theme(&self, theme: ThemeColors) {
        {
            let mut p = lock(&self.pushed);
            p.theme = Some(theme);
            p.themes += 1;
            p.since.get_or_insert_with(Instant::now);
        }
        self.request_frame();
    }

    /// (states, themes) pushed so far.
    pub(crate) fn push_counts(&self) -> (u64, u64) {
        let p = lock(&self.pushed);
        (p.states, p.themes)
    }

    pub(crate) fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.nudge();
    }
}

pub(crate) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Views not yet deallocated, and render threads not yet ended, for tests.
pub(crate) static LIVE_VIEWS: AtomicI64 = AtomicI64::new(0);
pub(crate) static LIVE_THREADS: AtomicI64 = AtomicI64::new(0);

thread_local! {
    /// On a render thread, the view it draws.
    static CURRENT: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
}

/// The view the calling render thread draws, if it is one.
pub(crate) fn current() -> Option<Arc<Shared>> {
    CURRENT.with(|c| c.borrow().clone())
}

pub(crate) fn current_id() -> Option<u64> {
    CURRENT.with(|c| c.borrow().as_ref().map(|s| s.id))
}

// ---------------------------------------------------------------------------
// Render thread
// ---------------------------------------------------------------------------

/// The Metal layer crosses to the render thread only to re-enable the drawable
/// timeout after wgpu configures the surface; CALayer properties are safe to
/// set off the main thread.
pub(crate) struct SendLayer(pub(crate) Retained<CAMetalLayer>);
unsafe impl Send for SendLayer {}

/// Start the render thread of the view SHARED describes.
pub(crate) fn spawn<A: EguiEmacsApp + 'static>(
    shared: Arc<Shared>,
    gpu: Arc<Gpu>,
    surface: wgpu::Surface<'static>,
    layer: SendLayer,
    make_app: fn() -> A,
) -> std::io::Result<thread::JoinHandle<()>> {
    let name = format!("emacs-egui-native-{}", shared.id);
    thread::Builder::new().name(name).spawn(move || {
        LIVE_THREADS.fetch_add(1, Ordering::SeqCst);
        let id = shared.id;
        CURRENT.with(|c| *c.borrow_mut() = Some(shared.clone()));
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            render_loop(&shared, gpu, surface, layer, make_app)
        }));
        if let Err(e) = result {
            let msg = panics::take_message(&*e);
            emit(id, "error", &format!(":message {}", lisp_string(&msg)));
        }
        CURRENT.with(|c| *c.borrow_mut() = None);
        LIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
    })
}

/// Take pending input for one frame. At most one pointer-button transition per
/// frame, so a press and a release delivered together still reach egui in two
/// passes, as a real click does; what is kept back asks for the next frame.
///
/// Keys wait while the view is focused but no egui widget has keyboard focus
/// yet, for at most three frames: an application gives a widget focus with
/// `request_focus`, which egui grants one frame later, and a key in between
/// would be dropped.
fn take_input(shared: &Shared) -> Vec<egui::Event> {
    let hold_keys = shared.focused.load(Ordering::SeqCst)
        && !shared.egui_focus.load(Ordering::SeqCst)
        && shared.focus_frames.load(Ordering::SeqCst) < 3;
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

fn clear_color(theme: &ThemeColors) -> wgpu::Color {
    // The surface is not sRGB, so egui's gamma-space colours go in unchanged.
    match parse_hex_color(&theme.bg) {
        Some(c) => wgpu::Color {
            r: c.r() as f64 / 255.0,
            g: c.g() as f64 / 255.0,
            b: c.b() as f64 / 255.0,
            a: 1.0,
        },
        None => wgpu::Color::BLACK,
    }
}

fn render_loop<A: EguiEmacsApp>(
    shared: &Arc<Shared>,
    gpu: Arc<Gpu>,
    surface: wgpu::Surface<'static>,
    layer: SendLayer,
    make_app: fn() -> A,
) {
    let ctx = egui::Context::default();
    let mut host = AppHost::new(make_app());
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
    let start = Instant::now();
    let mut report_at = Instant::now();
    let mut report_frames = 0u64;

    while wait_for_frame(shared) {
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

        // What Emacs pushed since the last frame.
        let (state, theme, states, themes, since) = {
            let mut p = lock(&shared.pushed);
            (
                p.state.take(),
                p.theme.take(),
                p.states,
                p.themes,
                p.since.take(),
            )
        };
        if let Some(theme) = theme {
            host.push_theme(theme);
        }
        if let Some(value) = state {
            match AppHost::<A>::decode(value) {
                Ok(state) => host.push_state(state),
                Err(m) => emit(
                    shared.id,
                    "error",
                    &format!(":message {}", lisp_string(&format!("state: {m}"))),
                ),
            }
        }

        let mut raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(geo.width, geo.height),
            )),
            time: Some(start.elapsed().as_secs_f64()),
            focused: true,
            events: take_input(shared),
            ..Default::default()
        };
        raw.viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point = Some(geo.scale);
        let mut applied = Applied::default();
        let output = ctx.run_ui(raw, |ui| {
            let a = host.frame(ui);
            applied.state |= a.state;
            applied.theme |= a.theme;
        });
        let laid_out = ctx.content_rect();
        *lock(&shared.egui_screen) = (laid_out.width(), laid_out.height());
        shared
            .egui_focus
            .store(ctx.memory(|m| m.focused().is_some()), Ordering::SeqCst);
        shared.focus_frames.fetch_add(1, Ordering::SeqCst);
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
                        load: wgpu::LoadOp::Clear(clear_color(host.theme())),
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
        if applied.state || applied.theme {
            let ms = since.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
            emit(
                shared.id,
                "painted",
                &format!(":push {states} :theme {themes} :ms {ms:.3} :frame {n}"),
            );
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
        let elapsed = report_at.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let fps = report_frames as f64 / elapsed.as_secs_f64();
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

/// Make a wgpu surface on LAYER.
pub(crate) fn surface_on(
    gpu: &Gpu,
    layer: &CAMetalLayer,
) -> anyhow::Result<wgpu::Surface<'static>> {
    unsafe {
        gpu.instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                layer as *const CAMetalLayer as *mut c_void,
            ))
    }
    .map_err(|e| anyhow::anyhow!("cannot make a wgpu surface: {e}"))
}
