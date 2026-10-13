//! The native backend's demo application, the UI its first prototypes drew: a frame
//! counter, the pushed `:label`, a button that counts clicks, a text field
//! that takes keys while the view is focused, and a bar that crosses its
//! track once a second while the pushed state has a non-nil `:animate`.
//!
//! It exports the prefix `emacs-egui-native` and names its view class
//! `EmacsEguiNativeView`, so the functions it defines and the class its views
//! have are the ones the native backend's checks read. It runs only natively: it uses
//! `emacs_egui_native::current` for what its checks read (`-info`'s
//! `:label`, which shows a non-string label as its JSON text, `:clicks`,
//! `:text`, `:text-focused`, `:animate`, `:button`, `:text-field`,
//! and `-test-click`'s targets `button` and `text`).

use std::time::Instant;

use emacs_egui_native::current;
use emacs_egui_sdk::{egui, emacs_post_message, EguiEmacsApp, ThemeColors};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize, Default)]
pub struct DemoState {
    #[serde(default)]
    label: Option<Value>,
    #[serde(default)]
    animate: Value,
}

pub struct DemoApp {
    state: DemoState,
    frame: u64,
    clicks: u64,
    text: String,
    last_text: String,
    start: Instant,
}

impl DemoApp {
    pub fn new() -> Self {
        Self {
            state: DemoState::default(),
            frame: 0,
            clicks: 0,
            text: String::new(),
            last_text: String::new(),
            start: Instant::now(),
        }
    }

    fn label(&self) -> String {
        match &self.state.label {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "(no :label pushed)".to_string(),
        }
    }

    fn animate(&self) -> bool {
        !matches!(self.state.animate, Value::Null | Value::Bool(false))
    }
}

fn rect_json(r: egui::Rect) -> Value {
    json!([r.left(), r.top(), r.width(), r.height()])
}

impl EguiEmacsApp for DemoApp {
    type State = DemoState;

    fn on_state_update(&mut self, state: DemoState) {
        self.state = state;
    }

    fn on_theme_update(&mut self, _theme: ThemeColors) {}

    fn update(&mut self, ui: &mut egui::Ui) {
        self.frame += 1;
        let bg = egui::Color32::from_rgb(0x28, 0x28, 0x28);
        ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
        let animate = self.animate();
        let focused = current::focused();
        let label = self.label();
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.style_mut().visuals = egui::Visuals::dark();
                ui.label(egui::RichText::new(format!("frame {}", self.frame)).monospace());
                ui.label(format!("state: {label}"));
                ui.horizontal(|ui| {
                    let button = ui.button(format!("click me ({})", self.clicks));
                    current::set_click_target("button", button.rect);
                    current::set_info("button", rect_json(button.rect));
                    if button.clicked() {
                        self.clicks += 1;
                        emacs_post_message("click", json!({ "count": self.clicks }));
                    }
                    ui.label(if focused { "keys: egui" } else { "keys: Emacs" });
                });
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.text)
                        .hint_text("focus the view, then type")
                        .desired_width(f32::INFINITY),
                );
                current::set_click_target("text", edit.rect);
                current::set_info("text-field", rect_json(edit.rect));
                current::set_info("text-focused", json!(edit.has_focus()));
                if focused && !edit.has_focus() {
                    edit.request_focus();
                } else if !focused && edit.has_focus() {
                    edit.surrender_focus();
                }
                if self.text != self.last_text {
                    self.last_text = self.text.clone();
                    emacs_post_message("text", json!({ "value": self.text }));
                }
                // A bar that crosses the track once a second while the pushed
                // state asks for the animation, and rests at the left otherwise.
                let (track, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 10.0),
                    egui::Sense::hover(),
                );
                let painter = ui.painter();
                painter.rect_filled(track, 3.0, egui::Color32::from_gray(60));
                let t = if animate {
                    self.start.elapsed().as_secs_f32() % 1.0
                } else {
                    0.0
                };
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
                if animate {
                    ui.ctx().request_repaint();
                }
            });
        current::set_info("label", json!(label));
        current::set_info("clicks", json!(self.clicks));
        current::set_info("text", json!(self.text));
        current::set_info("animate", json!(animate));
    }
}

// The prototypes named the view class EmacsEguiNativeView, and the backend's
// checks read that name as the first responder's.
emacs_egui_native::export_app!(
    DemoApp,
    prefix = "emacs-egui-native",
    DemoApp::new,
    class = "EmacsEguiNativeView"
);
