use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use wasm_bindgen::prelude::*;

// Re-export key dependencies so consumer crates don't need to declare them.
pub use egui;
pub use eframe;
pub use wasm_bindgen;
pub use wasm_bindgen_futures;
pub use js_sys;
pub use serde;
pub use serde_json;

lazy_static::lazy_static! {
    static ref REPAINT_SIGNAL: Mutex<Option<egui::Context>> = Mutex::new(None);
    static ref STATE_MESSAGE_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static ref THEME_MESSAGE_QUEUE: Mutex<Option<ThemeColors>> = Mutex::new(None);
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThemeColors {
    #[serde(default)]
    pub bg: String,
    #[serde(default)]
    pub fg: String,
    #[serde(rename = "font-size", default)]
    pub font_size: Option<f32>,
    #[serde(rename = "surface-bg", default)]
    pub surface_bg: String,
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self {
            bg: "#0c0c10".to_string(),
            fg: "#e6ebff".to_string(),
            font_size: None,
            surface_bg: String::new(),
        }
    }
}

/// Session parameters parsed from the URL hash fragment.
/// The Elisp runtime embeds these when creating the xwidget session.
#[derive(Clone, Debug)]
pub struct SessionParams {
    pub session_id: String,
    pub port: u16,
}

/// Parse session parameters (session ID and server port) from the URL hash fragment.
/// Returns defaults if parsing fails or running outside a browser.
pub fn parse_session_params() -> SessionParams {
    let (mut session_id, mut port) = ("default-session".to_string(), 8080u16);

    if let Some(win) = web_sys::window() {
        if let Ok(hash) = win.location().hash() {
            let hash_clean = hash.trim_start_matches('#');
            if let Ok(params) = web_sys::UrlSearchParams::new_with_str(hash_clean) {
                if let Some(s) = params.get("session") {
                    session_id = s;
                }
                if let Some(p) = params.get("port") {
                    if let Ok(parsed) = p.parse::<u16>() {
                        port = parsed;
                    }
                }
            }
        }
    }

    SessionParams { session_id, port }
}

/// Build the URL to fetch a local file through the Elisp HTTP server's file gateway.
///
/// The Elisp server exposes `/api/file?path=<encoded>` to stream local files
/// as raw bytes into the WASM sandbox.
pub fn file_url(path: &str) -> String {
    let params = parse_session_params();
    let encoded = js_sys::encode_uri_component(path);
    format!("http://127.0.0.1:{}/api/file?path={}", params.port, encoded)
}

/// Fetch raw bytes from a URL. Convenience wrapper around the browser fetch API.
///
/// Commonly used with [`file_url`] to load local files through the Elisp server:
/// ```ignore
/// let url = emacs_egui_sdk::file_url("/path/to/data.parquet");
/// let bytes = emacs_egui_sdk::fetch_bytes(&url).await?;
/// ```
pub async fn fetch_bytes(url: &str) -> Result<Vec<u8>, JsValue> {
    use wasm_bindgen::JsCast;
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let resp_value = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url)).await?;
    let resp: web_sys::Response = resp_value.dyn_into()?;

    if !resp.ok() {
        return Err(JsValue::from_str(&format!("HTTP status {}", resp.status())));
    }

    let array_buffer_value = wasm_bindgen_futures::JsFuture::from(resp.array_buffer()?).await?;
    let array_buffer: js_sys::ArrayBuffer = array_buffer_value.dyn_into()?;
    let typed_array = js_sys::Uint8Array::new(&array_buffer);

    let mut bytes = vec![0; typed_array.length() as usize];
    typed_array.copy_to(&mut bytes);
    Ok(bytes)
}

/// The EguiEmacsApp trait. Applications built with the emacs-egui framework
/// implement this trait instead of raw eframe::App.
pub trait EguiEmacsApp {
    type State: serde::de::DeserializeOwned;

    /// Called whenever Emacs pushes a new state payload.
    fn on_state_update(&mut self, state: Self::State);

    /// Called whenever Emacs pushes a theme update.
    fn on_theme_update(&mut self, theme: ThemeColors);

    /// Draws the interface (same as standard eframe::App).
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame);
}

/// Core wrapper struct that implements standard eframe::App and maps standard
/// message queues to the generic EguiEmacsApp implementation.
pub struct EguiEmacsAppWrapper<A: EguiEmacsApp> {
    app: A,
    current_theme: ThemeColors,
}

impl<A: EguiEmacsApp> EguiEmacsAppWrapper<A> {
    pub fn new(app: A) -> Self {
        Self {
            app,
            current_theme: ThemeColors::default(),
        }
    }
}

impl<A: EguiEmacsApp> eframe::App for EguiEmacsAppWrapper<A> {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Set context for repaints
        if let Ok(mut signal) = REPAINT_SIGNAL.lock() {
            if signal.is_none() {
                *signal = Some(ctx.clone());
            }
        }

        // 1. Drain incoming Lisp -> WASM state updates
        if let Ok(mut queue) = STATE_MESSAGE_QUEUE.lock() {
            for state_json in queue.drain(..) {
                if let Ok(state) = serde_json::from_str::<A::State>(&state_json) {
                    self.app.on_state_update(state);
                }
            }
        }

        // 2. Drain incoming Lisp -> WASM theme updates
        let mut theme_updated = false;
        if let Ok(mut opt) = THEME_MESSAGE_QUEUE.lock() {
            if let Some(theme) = opt.take() {
                self.current_theme = theme;
                theme_updated = true;
            }
        }

        if theme_updated {
            apply_theme_to_ctx(ctx, &self.current_theme);
            self.app.on_theme_update(self.current_theme.clone());
        }

        // 3. Delegate to user application
        self.app.update(ctx, frame);
    }
}

/// Helper to parse a hex color into an egui Color32
pub fn parse_hex_color(hex: &str) -> Option<egui::Color32> {
    let hex = hex.trim_start_matches('#');
    if hex.len() >= 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        Some(egui::Color32::from_rgb(r, g, b))
    } else {
        None
    }
}

fn luminance(c: egui::Color32) -> u8 {
    ((c.r() as u32 * 299 + c.g() as u32 * 587 + c.b() as u32 * 114) / 1000) as u8
}

fn apply_theme_to_ctx(ctx: &egui::Context, theme: &ThemeColors) {
    let bg = parse_hex_color(&theme.bg).unwrap_or(egui::Color32::from_rgb(12, 12, 16));
    let fg = parse_hex_color(&theme.fg).unwrap_or(egui::Color32::from_rgb(230, 235, 255));
    let is_dark = luminance(bg) < 128;

    let mut style = (*ctx.style()).clone();

    // Setup general spacing & sizing
    let text_size = theme
        .font_size
        .map(|sz| sz * 0.76)
        .unwrap_or(12.0)
        .clamp(10.0, 14.0);

    style.override_text_style = Some(egui::TextStyle::Body);

    // Set custom fonts sizes
    for (_, font_id) in style.text_styles.iter_mut() {
        font_id.size = text_size;
        font_id.family = egui::FontFamily::Monospace;
    }

    // Configure visuals
    let mut visuals = if is_dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    visuals.widgets.noninteractive.bg_fill = parse_hex_color(&theme.surface_bg).unwrap_or(bg);
    visuals.widgets.noninteractive.fg_stroke.color = fg;
    visuals.window_fill = parse_hex_color(&theme.surface_bg).unwrap_or(bg);
    visuals.panel_fill = parse_hex_color(&theme.surface_bg).unwrap_or(bg);

    style.visuals = visuals;
    ctx.set_style(style);
}

/// Posts an action message back to Emacs via two channels:
/// 1. Mutates `document.title` (fallback for macOS title-change hook)
/// 2. Non-blocking loopback HTTP fetch to the Elisp server
pub fn emacs_post_message<T: Serialize>(action: &str, payload: T) {
    if let Ok(payload_json) = serde_json::to_string(&payload) {
        // 1. Mutate document.title as a fallback / visual hook indicator
        let msg = serde_json::json!({
            "action": action,
            "payload": payload
        });
        if let Ok(json_str) = serde_json::to_string(&msg) {
            if let Some(win) = web_sys::window() {
                if let Some(doc) = win.document() {
                    doc.set_title(&json_str);
                }
            }
        }

        // 2. Non-blocking loopback fetch using parsed session params.
        let params = parse_session_params();
        let encoded_payload = js_sys::encode_uri_component(&payload_json);
        let url = format!(
            "http://127.0.0.1:{}/api/event?session={}&action={}&payload={}",
            params.port, params.session_id, action, encoded_payload
        );

        wasm_bindgen_futures::spawn_local(async move {
            if let Some(win) = web_sys::window() {
                let _ = win.fetch_with_str(&url);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// JS-Exposed WASM Bindings
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn egui_push_state(json: &str) {
    if let Ok(mut queue) = STATE_MESSAGE_QUEUE.lock() {
        queue.push(json.to_string());
    }
    // Trigger repaint immediately
    if let Ok(signal) = REPAINT_SIGNAL.lock() {
        if let Some(ctx) = &*signal {
            ctx.request_repaint();
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn egui_push_theme(json: &str) {
    if let Ok(theme) = serde_json::from_str::<ThemeColors>(json) {
        if let Ok(mut guard) = THEME_MESSAGE_QUEUE.lock() {
            *guard = Some(theme);
        }
        if let Ok(signal) = REPAINT_SIGNAL.lock() {
            if let Some(ctx) = &*signal {
                ctx.request_repaint();
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub fn bootstrap_app<A: EguiEmacsApp + 'static>(app: A, canvas_id: &str) -> Result<(), JsValue> {
    // Redirect panics to browser console
    console_error_panic_hook::set_once();

    let web_options = eframe::WebOptions::default();
    let canvas_id = canvas_id.to_string();

    wasm_bindgen_futures::spawn_local(async move {
        let document = web_sys::window()
            .and_then(|win| win.document())
            .expect("Failed to get document");
        let canvas = document
            .get_element_by_id(&canvas_id)
            .expect("Failed to get canvas")
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("Failed to cast to canvas");

        let runner = eframe::WebRunner::new();
        runner.start(
            canvas,
            web_options,
            Box::new(|_cc| Ok(Box::new(EguiEmacsAppWrapper::new(app)))),
        )
        .await
        .expect("failed to start eframe");
    });

    Ok(())
}

/// High-level entry point that parses session parameters from the URL hash
/// and bootstraps the app. Use this from your `#[wasm_bindgen]` entry point:
///
/// ```ignore
/// #[wasm_bindgen]
/// pub fn start_app(canvas_id: &str) -> Result<(), JsValue> {
///     emacs_egui_sdk::launch(canvas_id, |session, port| MyApp::new(session, port))
/// }
/// ```
///
/// If your app doesn't need session/port, use [`launch_simple`] instead.
#[cfg(target_arch = "wasm32")]
pub fn launch<A, F>(canvas_id: &str, ctor: F) -> Result<(), JsValue>
where
    A: EguiEmacsApp + 'static,
    F: FnOnce(SessionParams) -> A,
{
    let params = parse_session_params();
    bootstrap_app(ctor(params), canvas_id)
}

/// Simplified entry point for apps that don't need session parameters:
///
/// ```ignore
/// #[wasm_bindgen]
/// pub fn start_app(canvas_id: &str) -> Result<(), JsValue> {
///     emacs_egui_sdk::launch_simple(canvas_id, MyApp::new())
/// }
/// ```
#[cfg(target_arch = "wasm32")]
pub fn launch_simple<A: EguiEmacsApp + 'static>(canvas_id: &str, app: A) -> Result<(), JsValue> {
    bootstrap_app(app, canvas_id)
}
