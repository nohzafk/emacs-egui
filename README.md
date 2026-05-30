# emacs-egui

A generic, high-performance, GPU-accelerated framework for running **egui** immediate-mode interfaces compiled to WebAssembly smoothly inside Emacs buffers and child frames.

By combining the speed of the **Rust Arrow/egui ecosystem**, the rendering power of **WebGL/WebGPU in WebKit**, and **Emacs’s buffer/frame lifecycle**, this framework enables developers to build desktop-grade visual tools inside Emacs without needing heavy external runtimes (like Python, PyQt, or Deno).

```text
  +-------------------------------------------------------------+
  | Emacs Host (Elisp)                                          |
  | - HTTP Server (hosts WASM bundles under /app/app-name/)     |
  | - Frame/Buffer Manager (Xwidget containers)                 |
  | - IPC Listener (high-performance loopback router)           |
  +------------------------------+------------------------------+
                                 |  Lisp-to-Rust JSON pushes
                                 |  Rust-to-Lisp loopback fetch
                                 v
  +-------------------------------------------------------------+
  | Rust WASM SDK (`emacs-egui-sdk`)                            |
  | - `EguiEmacsApp` Trait (boilerplate-free state sync)        |
  | - Auto-Theming Adapter (maps Emacs colors to egui Visuals)  |
  | - Event Emitter (WASM -> loopback IPC -> Elisp)             |
  +-------------------------------------------------------------+
```

---

## Key Features

1. **Pure Elisp Local Server:** Runs a TCP socket HTTP server entirely in-process (`make-network-process`) bound to loopback `127.0.0.1`. It dynamically routes and hosts WASM bundles under `/app/<app-name>/` with zero external web daemons or CDNs.
2. **Secure Binary File Gateway:** Bypasses standard browser/WebKit sandbox restrictions securely. The Elisp server exposes `/api/file?path=<encoded_path>` to stream large OS-level files (like `.parquet`, `.csv`, `.json`) directly into the WebAssembly application as raw binary streams.
3. **Synchronized Bidirectional IPC:**
    * **Lisp $\rightarrow$ WASM:** Pushes state changes dynamically by evaluating `window.eguiPushState(json_string)` using `xwidget-webkit-execute-script`.
    * **WASM $\rightarrow$ Lisp:** Dispatches events using a non-blocking `fetch` request to `/api/event?session=id&action=name&payload=json`. The Elisp HTTP filter intercepts the request, runs registered hooks asynchronously, and returns `200 OK`.
4. **Zero-Flash Theme Syncing:** Emacs read active theme colors (background, foreground, and font heights) and passes them to the WASM app via a URL fragment on boot, preventing unstyled light/dark flashes. It updates them dynamically on theme switches using `window.eguiPushTheme`.

---

## Crate: `emacs-egui-sdk`

The Rust SDK abstracts away browser integrations and WebAssembly compiler setups. Developers implement the `EguiEmacsApp` trait instead of raw `eframe::App`:

```rust
use emacs_egui_sdk::{EguiEmacsApp, ThemeColors};

pub struct MyApplication {
    filepath: String,
}

impl EguiEmacsApp for MyApplication {
    type State = MyState;

    fn on_state_update(&mut self, state: Self::State) {
        self.filepath = state.filepath;
    }

    fn on_theme_update(&mut self, theme: ThemeColors) {
        // Automatically applied to egui::Visuals by the SDK wrapper,
        // hook here to customize application specific accents.
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label(format!("Viewing: {}", self.filepath));
        });
    }
}
```

To bind back to Emacs, post messages cleanly using:

```rust
emacs_egui_sdk::emacs_post_message("item-selected", serde_json::json!({ "id": 100 }));
```

And bootstrap inside `#[cfg(target_arch = "wasm32")]`:

```rust
#[wasm_bindgen]
pub fn start_app(canvas_id: &str) -> Result<(), JsValue> {
    emacs_egui_sdk::bootstrap_app(MyApplication::new(), canvas_id)
}
```

---

## Repository Structure

```text
emacs-egui/
├── lisp/
│   └── emacs-egui.el     # Core package: TCP socket server + container manager + IPC
└── sdk/                  # The generic Rust egui/WASM SDK
    ├── Cargo.toml
    └── src/
        └── lib.rs        # Auto-theming, trait definitions, and loopback handlers
```

---

## License

This project is licensed under the MIT License.
