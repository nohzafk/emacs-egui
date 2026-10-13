//! The emacs-egui SDK: what an egui application is written against, whichever
//! backend draws it.
//!
//! An application implements [`EguiEmacsApp`] once. Two backends host it:
//!
//! - **web** (cargo feature `web`): the application is compiled to
//!   WebAssembly and runs under eframe in an `xwidget-webkit` page. Emacs
//!   pushes JSON through the JavaScript entry points `egui_push_state` and
//!   `egui_push_theme`, and [`emacs_post_message`] answers over the Elisp
//!   server's loopback route.
//! - **native**: the `emacs-egui-native` crate links the application into a
//!   dynamic module and renders it with Metal in a view that Emacs places. It
//!   installs the route for [`emacs_post_message`] with [`set_message_sink`].
//!
//! Both drive the application through an [`AppHost`], so an application sees
//! the same state type on either: the web backend parses the pushed JSON into
//! a [`serde_json::Value`], the native one converts the pushed Lisp plist into
//! the same `Value`, and [`AppHost::decode`] deserializes it into
//! [`EguiEmacsApp::State`].

use serde::{Deserialize, Deserializer, Serialize};
use std::sync::OnceLock;

// Re-export key dependencies so application crates need not declare them.
pub use egui;
pub use serde;
pub use serde_json;

#[cfg(feature = "web")]
mod web;
#[cfg(feature = "web")]
pub use web::*;

/// The Emacs theme a widget follows: the frame's default face, resolved on
/// the frame that displays the widget.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ThemeColors {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub bg: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub fg: String,
    #[serde(rename = "font-size", default)]
    pub font_size: Option<f32>,
    #[serde(rename = "surface-bg", default, deserialize_with = "null_as_empty")]
    pub surface_bg: String,
    #[serde(rename = "font-family", default)]
    pub font_family: Option<String>,
}

/// A JSON `null`, which a page sends for a parameter its URL lacks, reads as
/// an empty string instead of failing the whole theme.
fn null_as_empty<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self {
            bg: "#0c0c10".to_string(),
            fg: "#e6ebff".to_string(),
            font_size: None,
            surface_bg: String::new(),
            font_family: None,
        }
    }
}

/// An application hosted in Emacs.
pub trait EguiEmacsApp {
    /// The state Emacs pushes: a plist on the Emacs side, deserialized from
    /// the JSON value it maps to (keys without their colon).
    type State: serde::de::DeserializeOwned;

    /// Called with each state Emacs pushes, before the frame that shows it.
    fn on_state_update(&mut self, state: Self::State);

    /// Called when Emacs pushes a theme, after the SDK has applied it to the
    /// context's style ([`apply_theme_to_ctx`]).
    fn on_theme_update(&mut self, theme: ThemeColors);

    /// Draw one frame into `ui`, the root `Ui` that covers the whole widget,
    /// with no margin or background. Its context is `ui.ctx()`.
    fn update(&mut self, ui: &mut egui::Ui);
}

/// What one frame applied, for a backend that reports the frame that showed
/// a push.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub state: bool,
    pub theme: bool,
}

/// One application as a backend drives it: the state and theme pushed to it,
/// counted, and its frames.
pub struct AppHost<A: EguiEmacsApp> {
    app: A,
    theme: ThemeColors,
    pending_theme: Option<ThemeColors>,
    applied: Applied,
    pushes: u64,
    themes: u64,
}

impl<A: EguiEmacsApp> AppHost<A> {
    pub fn new(app: A) -> Self {
        Self {
            app,
            theme: ThemeColors::default(),
            pending_theme: None,
            applied: Applied::default(),
            pushes: 0,
            themes: 0,
        }
    }

    /// Deserialize a pushed value into the application's state type; an
    /// error names where in the value it failed, as `sections[0].priority`.
    pub fn decode(value: serde_json::Value) -> Result<A::State, String> {
        serde_path_to_error::deserialize(value).map_err(|e| {
            let at = e.path().to_string();
            if at == "." {
                e.into_inner().to_string()
            } else {
                format!("{}, at {at}", e.into_inner())
            }
        })
    }

    /// Hand a state to the application now.
    pub fn push_state(&mut self, state: A::State) {
        self.app.on_state_update(state);
        self.pushes += 1;
        self.applied.state = true;
    }

    /// Keep a theme for the next frame, which applies it before drawing.
    pub fn push_theme(&mut self, theme: ThemeColors) {
        self.pending_theme = Some(theme);
        self.themes += 1;
    }

    /// Run one frame: apply a pending theme, then let the application draw.
    pub fn frame(&mut self, ui: &mut egui::Ui) -> Applied {
        if let Some(theme) = self.pending_theme.take() {
            apply_theme_to_ctx(ui.ctx(), &theme);
            // The root `Ui` copied the style when the pass began; give it the
            // new one, so this frame already draws in the new theme.
            ui.set_style(ui.ctx().global_style());
            self.theme = theme.clone();
            self.app.on_theme_update(theme);
            self.applied.theme = true;
        }
        self.app.update(ui);
        std::mem::take(&mut self.applied)
    }

    /// The theme last applied.
    pub fn theme(&self) -> &ThemeColors {
        &self.theme
    }

    /// How many states have been pushed.
    pub fn pushes(&self) -> u64 {
        self.pushes
    }

    /// How many themes have been pushed.
    pub fn themes(&self) -> u64 {
        self.themes
    }

    pub fn app(&self) -> &A {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut A {
        &mut self.app
    }
}

/// Parse `#rrggbb` (anything after the first six digits is ignored).
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

/// Map the Emacs theme onto the context's style: dark or light visuals by
/// the background's luminance, the background as panel and window fill, the
/// foreground as text, and one monospace text size derived from the face.
pub fn apply_theme_to_ctx(ctx: &egui::Context, theme: &ThemeColors) {
    let bg = parse_hex_color(&theme.bg).unwrap_or(egui::Color32::from_rgb(12, 12, 16));
    let fg = parse_hex_color(&theme.fg).unwrap_or(egui::Color32::from_rgb(230, 235, 255));
    let is_dark = luminance(bg) < 128;

    let mut style = (*ctx.global_style()).clone();

    let text_size = theme
        .font_size
        .map(|sz| sz * 0.76)
        .unwrap_or(12.0)
        .clamp(10.0, 14.0);

    style.override_text_style = Some(egui::TextStyle::Body);

    for (_, font_id) in style.text_styles.iter_mut() {
        font_id.size = text_size;
        font_id.family = egui::FontFamily::Monospace;
    }

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
    ctx.set_global_style(style);
}

/// Where [`emacs_post_message`] sends a message when a backend other than the
/// web one hosts the application.
pub type MessageSink = fn(action: &str, payload: serde_json::Value);

static MESSAGE_SINK: OnceLock<MessageSink> = OnceLock::new();

/// Route [`emacs_post_message`] to `sink`. A backend calls this once; false
/// if a sink was already set.
pub fn set_message_sink(sink: MessageSink) -> bool {
    MESSAGE_SINK.set(sink).is_ok()
}

/// Send an action and its payload to Emacs, where the host's function for
/// that action receives the payload as a plist.
///
/// The native backend's sink delivers it through the module's pipe process;
/// without a sink, the web backend fetches the Elisp server's loopback route.
/// Without either, the message is dropped.
pub fn emacs_post_message<T: Serialize>(action: &str, payload: T) {
    let Ok(payload) = serde_json::to_value(&payload) else {
        return;
    };
    if let Some(sink) = MESSAGE_SINK.get() {
        sink(action, payload);
        return;
    }
    #[cfg(feature = "web")]
    web::post_message(action, &payload);
}
