//! The web backend's half of the SDK: the eframe bootstrap, the JavaScript
//! entry points the Elisp runtime calls, and the loopback route back to it.
//!
//! The page receives its theme and session in the URL fragment
//! (`#bg=..&fg=..&font-size=..&session=..&port=..`). Emacs pushes through
//! `window.eguiPushState(json)` and `window.eguiPushTheme(json)`, which the
//! page binds to [`egui_push_state`] and [`egui_push_theme`]; both are queued
//! and drained on the next frame. The SDK posts `ready` after the first frame
//! and `painted` (`push`, `theme`: how many of each the application has
//! received) after each frame that applied a push or a theme.

use std::sync::{Mutex, MutexGuard};

use serde_json::json;
use wasm_bindgen::prelude::*;

use crate::{emacs_post_message, AppHost, EguiEmacsApp, ThemeColors};

pub use eframe;
pub use js_sys;
pub use wasm_bindgen;
pub use wasm_bindgen_futures;

static REPAINT_SIGNAL: Mutex<Option<egui::Context>> = Mutex::new(None);
static STATE_MESSAGE_QUEUE: Mutex<Vec<String>> = Mutex::new(Vec::new());
static THEME_MESSAGE_QUEUE: Mutex<Option<ThemeColors>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

/// The web route of [`emacs_post_message`], through two channels:
/// 1. `document.title`, for Emacs builds that run a title-change hook;
/// 2. a non-blocking loopback fetch of the Elisp server's `/api/event`.
pub(crate) fn post_message(action: &str, payload: &serde_json::Value) {
    let Ok(payload_json) = serde_json::to_string(payload) else {
        return;
    };
    let msg = json!({ "action": action, "payload": payload });
    if let Some(doc) = web_sys::window().and_then(|win| win.document()) {
        doc.set_title(&msg.to_string());
    }

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

/// The eframe application that feeds an [`EguiEmacsApp`] from the queues the
/// JavaScript entry points fill.
pub struct EguiEmacsAppWrapper<A: EguiEmacsApp> {
    host: AppHost<A>,
    ready: bool,
}

impl<A: EguiEmacsApp> EguiEmacsAppWrapper<A> {
    pub fn new(app: A) -> Self {
        Self {
            host: AppHost::new(app),
            ready: false,
        }
    }
}

impl<A: EguiEmacsApp> eframe::App for EguiEmacsAppWrapper<A> {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::TRANSPARENT.to_normalized_gamma_f32()
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut signal = lock(&REPAINT_SIGNAL);
        if signal.is_none() {
            *signal = Some(ctx.clone());
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let states = std::mem::take(&mut *lock(&STATE_MESSAGE_QUEUE));
        for state_json in states {
            let decoded = serde_json::from_str::<serde_json::Value>(&state_json)
                .map_err(|e| e.to_string())
                .and_then(AppHost::<A>::decode);
            match decoded {
                Ok(state) => self.host.push_state(state),
                Err(message) => {
                    emacs_post_message("error", json!({ "message": format!("state: {message}") }))
                }
            }
        }
        if let Some(theme) = lock(&THEME_MESSAGE_QUEUE).take() {
            self.host.push_theme(theme);
        }

        let applied = self.host.frame(ui);

        // eframe paints this frame when `ui` returns, in the same animation
        // frame; the fetch these spawn runs after that.
        if !self.ready {
            self.ready = true;
            emacs_post_message("ready", json!({}));
        }
        if applied.state || applied.theme {
            emacs_post_message(
                "painted",
                json!({ "push": self.host.pushes(), "theme": self.host.themes() }),
            );
        }
    }
}

fn request_repaint() {
    if let Some(ctx) = &*lock(&REPAINT_SIGNAL) {
        ctx.request_repaint();
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn egui_push_state(json: &str) {
    lock(&STATE_MESSAGE_QUEUE).push(json.to_string());
    request_repaint();
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub fn egui_push_theme(json: &str) {
    match serde_json::from_str::<ThemeColors>(json) {
        Ok(theme) => {
            *lock(&THEME_MESSAGE_QUEUE) = Some(theme);
            request_repaint();
        }
        Err(e) => emacs_post_message("error", json!({ "message": format!("theme: {e}") })),
    }
}

#[cfg(target_arch = "wasm32")]
pub fn bootstrap_app<A: EguiEmacsApp + 'static>(app: A, canvas_id: &str) -> Result<(), JsValue> {
    use wasm_bindgen::JsCast;

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
        if let Err(e) = runner
            .start(
                canvas,
                web_options,
                Box::new(|_cc| Ok(Box::new(EguiEmacsAppWrapper::new(app)))),
            )
            .await
        {
            emacs_post_message(
                "error",
                json!({ "message": format!("eframe did not start: {e:?}") }),
            );
        }
    });

    Ok(())
}

/// High-level entry point that parses session parameters from the URL hash
/// and bootstraps the app. Use this from your `#[wasm_bindgen]` entry point:
///
/// ```ignore
/// #[wasm_bindgen]
/// pub fn start_app(canvas_id: &str) -> Result<(), JsValue> {
///     emacs_egui_sdk::launch(canvas_id, |session| MyApp::new(session))
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
