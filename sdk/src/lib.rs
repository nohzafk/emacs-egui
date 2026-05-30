use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use wasm_bindgen::prelude::*;

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

/// The EguiEmacsApp trait. Applications built inside the emacs-egui framework
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

/// Posts an action message back to Emacs by mutating document.title
#[derive(Serialize)]
struct EmacsMessage<T: Serialize> {
    action: String,
    payload: T,
}

pub fn emacs_post_message<T: Serialize>(action: &str, payload: T) {
    let msg = EmacsMessage {
        action: action.to_string(),
        payload,
    };
    if let Ok(json_str) = serde_json::to_string(&msg) {
        if let Some(win) = web_sys::window() {
            if let Some(doc) = win.document() {
                doc.set_title(&json_str);
            }
        }
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
