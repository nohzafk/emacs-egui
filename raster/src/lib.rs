//! emacs-egui-raster: an Emacs dynamic module that renders SVG to PNG with
//! resvg.
//!
//! Emacs draws an `svg` image with the engine it was built with: librsvg, or,
//! on macOS without librsvg, AppKit's own SVG support, which implements less
//! of SVG (no markers, no `dy` on text rows, a part of CSS). This module
//! renders with resvg, a renderer written in Rust, and hands Emacs a PNG,
//! which every build of Emacs shows the same way.
//!
//! ```elisp
//! (emacs-egui-raster-svg SVG &key WIDTH HEIGHT SCALE DEFAULT-FONT) ; -> PNG bytes
//! (emacs-egui-raster-svg-size SVG)                                  ; -> (WIDTH . HEIGHT)
//! (emacs-egui-raster-fonts &optional FAMILIES)                      ; -> plist
//! ```
//!
//! Sizes are in points, the unit of an SVG's own `width` and `height` and of
//! Emacs's pixels on a display that scales; SCALE is the number of device
//! pixels per point, so a PNG for a 2x display is twice as wide and tall as
//! the box it is fitted into. The font database loads the system fonts once,
//! when the module is loaded, and every call shares it.
//!
//! An `<image>` element may embed a `data:` URL; it never reads a file.

use std::{
    cell::RefCell,
    panic::{self, AssertUnwindSafe},
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use emacs::{defun, Env, IntoLisp, Result, Value};

emacs::plugin_is_GPL_compatible!();

/// The functions whose Lisp signature the `emacs` crate cannot express
/// (`&rest`, `&optional`), as Lisp wrappers around the fixed-arity functions
/// defined below.
const WRAPPERS: &[&str] = &[
    r#"(defun emacs-egui-raster-svg (svg &rest options)
  "Render SVG, a string, to PNG and return the PNG as a unibyte string.
OPTIONS are keyword arguments:

:width, :height  The box, in points, that the image is fitted inside
                 with its aspect ratio kept.  A missing one does not
                 constrain; with neither, the image has the SVG's own
                 size.
:scale           Device pixels per point, 1.0 when missing.  The PNG is
                 the fitted size times SCALE, in pixels.
:default-font    The font family of text that names none.

Signal `emacs-egui-raster-error' with resvg's message when SVG cannot be
parsed.

\(fn SVG &key WIDTH HEIGHT SCALE DEFAULT-FONT)"
  (emacs-egui-raster--svg svg options))"#,
    r#"(defun emacs-egui-raster-fonts (&optional families)
  "Describe the font database every render shares.
Return a plist:

:faces       the number of faces it loaded;
:load-ms     how long loading the system fonts took, in milliseconds;
:monospace, :sans-serif, :serif
             for each generic family, (NAME FAMILY POSTSCRIPT FILE):
             the family the database maps it to and the face a query
             for it finds, or (NAME) when none matches;
:families    the same for each name in FAMILIES, a list of strings.

A text whose families all fail to match is drawn with the serif face."
  (emacs-egui-raster--fonts families))"#,
];

#[emacs::module(
    name = "emacs-egui-raster",
    defun_prefix = "emacs-egui-raster",
    separator = "-"
)]
fn init(env: &Env) -> Result<()> {
    install_panic_hook();
    fonts();
    env.define_error(
        env.intern("emacs-egui-raster-error")?,
        "SVG raster error",
        (env.intern("error")?,),
    )?;
    for source in WRAPPERS {
        let form = env.call("read", (*source,))?;
        env.call("eval", (form, env.intern("t")?))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Panics
// ---------------------------------------------------------------------------

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

/// Record each panic's message and location for `guard`, then run the
/// previous hook.
fn install_panic_hook() {
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

fn take_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    LAST_PANIC
        .with(|p| p.borrow_mut().take())
        .unwrap_or_else(|| payload_message(payload))
}

/// Run a Lisp function's body. A panic in it goes on to the `emacs` crate's
/// boundary, which signals `rust-panic`, but re-raised with a `String`
/// payload: the crate reads the message only from a `String`, and a literal
/// `panic!` or an `unwrap` on `None` carries a `&str`, which would reach Lisp
/// as "Any { .. }".
fn guard<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => {
            let message = take_panic_message(&*payload);
            panic::resume_unwind(Box::new(format!(
                "Rust panic in emacs-egui-raster: {message}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

struct Fonts {
    db: Arc<fontdb::Database>,
    load_ms: f64,
}

static FONTS: OnceLock<Fonts> = OnceLock::new();

/// The shared font database; `init` loads it with the system fonts.
fn fonts() -> &'static Fonts {
    FONTS.get_or_init(|| {
        let started = Instant::now();
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        Fonts {
            db: Arc::new(db),
            load_ms: ms(started),
        }
    })
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

/// (NAME FAMILY POSTSCRIPT FILE) for the face a query for FAMILY finds, or
/// (NAME) when none matches.
fn describe<'e>(env: &'e Env, name: &str, family: fontdb::Family) -> Result<Value<'e>> {
    let db = &fonts().db;
    let query = fontdb::Query {
        families: &[family],
        ..Default::default()
    };
    match db.query(&query).and_then(|id| db.face(id)) {
        None => env.list((name,)),
        Some(face) => {
            let file = match &face.source {
                fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
                    path.display().to_string().into_lisp(env)?
                }
                fontdb::Source::Binary(_) => env.intern("nil")?,
            };
            let resolved = face
                .families
                .first()
                .map(|(f, _)| f.as_str())
                .unwrap_or("");
            env.list((name, resolved, face.post_script_name.as_str(), file))
        }
    }
}

/// Describe the font database (internal; see `emacs-egui-raster-fonts').
#[defun(name = "-fonts")]
fn fonts_info<'e>(env: &'e Env, families: Value<'e>) -> Result<Value<'e>> {
    guard(|| {
        let f = fonts();
        let generic = |g: fontdb::Family| -> Result<Value<'e>> {
            describe(env, f.db.family_name(&g), g)
        };
        let mut named = Vec::new();
        let mut rest = families;
        while rest.is_not_nil() {
            let name: String = rest.car::<Value>()?.into_rust()?;
            named.push(name);
            rest = rest.cdr()?;
        }
        let mut described = Vec::new();
        for name in &named {
            described.push(describe(env, name, fontdb::Family::Name(name))?);
        }
        let families = env.call("list", &described[..])?;
        env.list((
            env.intern(":faces")?,
            f.db.len() as i64,
            env.intern(":load-ms")?,
            f.load_ms,
            env.intern(":monospace")?,
            generic(fontdb::Family::Monospace)?,
            env.intern(":sans-serif")?,
            generic(fontdb::Family::SansSerif)?,
            env.intern(":serif")?,
            generic(fontdb::Family::Serif)?,
            env.intern(":families")?,
            families,
        ))
    })
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The most pixels one PNG may have: 16384 x 16384, 1 GiB of RGBA.
const MAX_PIXELS: u64 = 1 << 28;

/// What the last render cost, for `emacs-egui-raster--last-timing'.
#[derive(Clone, Copy, Default)]
struct Timing {
    parse_ms: f64,
    render_ms: f64,
    encode_ms: f64,
    width: u32,
    height: u32,
}

static LAST_TIMING: Mutex<Timing> = Mutex::new(Timing {
    parse_ms: 0.0,
    render_ms: 0.0,
    encode_ms: 0.0,
    width: 0,
    height: 0,
});

fn raster_error<T>(env: &Env, message: String) -> Result<T> {
    env.signal(env.intern("emacs-egui-raster-error")?, (message,))
}

fn options(default_font: Option<String>) -> usvg::Options<'static> {
    let mut opt = usvg::Options {
        fontdb: fonts().db.clone(),
        ..Default::default()
    };
    opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
    if let Some(family) = default_font {
        opt.font_family = family;
    }
    opt
}

fn parse(env: &Env, svg: Value, default_font: Option<String>) -> Result<usvg::Tree> {
    let bytes: Vec<u8> = svg.into_rust()?;
    match usvg::Tree::from_data(&bytes, &options(default_font)) {
        Ok(tree) => Ok(tree),
        Err(e) => raster_error(env, format!("cannot parse the SVG: {e}")),
    }
}

/// A positive finite number from VALUE, an integer or a float.
fn positive(env: &Env, key: &str, value: Value) -> Result<f64> {
    let x: f64 = env.call("float", (value,))?.into_rust()?;
    if x.is_finite() && x > 0.0 {
        Ok(x)
    } else {
        raster_error(env, format!("{key} must be a positive number, not {x}"))
    }
}

#[derive(Default)]
struct Request {
    width: Option<f64>,
    height: Option<f64>,
    scale: Option<f64>,
    default_font: Option<String>,
}

fn request(env: &Env, mut plist: Value) -> Result<Request> {
    let mut r = Request::default();
    while plist.is_not_nil() {
        let key: Value = plist.car()?;
        let tail: Value = plist.cdr()?;
        let name: String = env.call("symbol-name", (key,))?.into_rust()?;
        if !tail.is_not_nil() {
            return raster_error(env, format!("{name} has no value"));
        }
        let value: Value = tail.car()?;
        if value.is_not_nil() {
            match name.as_str() {
                ":width" => r.width = Some(positive(env, &name, value)?),
                ":height" => r.height = Some(positive(env, &name, value)?),
                ":scale" => r.scale = Some(positive(env, &name, value)?),
                ":default-font" => r.default_font = Some(value.into_rust()?),
                _ => return raster_error(env, format!("unknown keyword {name}")),
            }
        }
        plist = tail.cdr()?;
    }
    Ok(r)
}

/// Render SVG with OPTIONS, a plist (internal; see `emacs-egui-raster-svg').
#[defun(name = "-svg")]
fn svg<'e>(env: &'e Env, svg: Value<'e>, options: Value<'e>) -> Result<Value<'e>> {
    guard(|| {
        let r = request(env, options)?;
        let started = Instant::now();
        let tree = parse(env, svg, r.default_font)?;
        let parse_ms = ms(started);

        let size = tree.size();
        let (sw, sh) = (size.width() as f64, size.height() as f64);
        let fit = match (r.width, r.height) {
            (None, None) => 1.0,
            (Some(w), None) => w / sw,
            (None, Some(h)) => h / sh,
            (Some(w), Some(h)) => (w / sw).min(h / sh),
        };
        let scale = r.scale.unwrap_or(1.0);
        let pw = (sw * fit * scale).round().max(1.0);
        let ph = (sh * fit * scale).round().max(1.0);
        if pw * ph > MAX_PIXELS as f64 {
            return raster_error(
                env,
                format!("{pw} x {ph} pixels is more than {MAX_PIXELS}"),
            );
        }
        let (pw, ph) = (pw as u32, ph as u32);

        let started = Instant::now();
        let Some(mut pixmap) = tiny_skia::Pixmap::new(pw, ph) else {
            return raster_error(env, format!("cannot make a {pw} x {ph} pixmap"));
        };
        let transform =
            tiny_skia::Transform::from_scale((pw as f64 / sw) as f32, (ph as f64 / sh) as f32);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let render_ms = ms(started);

        let started = Instant::now();
        let png = match encode(pixmap) {
            Ok(png) => png,
            Err(e) => return raster_error(env, format!("cannot encode the PNG: {e}")),
        };
        let encode_ms = ms(started);

        *LAST_TIMING.lock().unwrap_or_else(|e| e.into_inner()) = Timing {
            parse_ms,
            render_ms,
            encode_ms,
            width: pw,
            height: ph,
        };
        png.into_lisp(env)
    })
}

/// PIXMAP as PNG bytes, with the png crate's fast deflate. On diagrams it
/// encodes about three times faster than the balanced level tiny-skia's own
/// `encode_png` uses, for PNGs 1.5 to 3 times larger; a host that draws the
/// image again on every theme change or resize pays the time each time and
/// the size once.
fn encode(pixmap: tiny_skia::Pixmap) -> std::result::Result<Vec<u8>, png::EncodingError> {
    let (width, height) = (pixmap.width(), pixmap.height());
    let data = pixmap.take_demultiplied();
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&data)?;
    }
    Ok(out)
}

/// The size of SVG, a string, in points: (WIDTH . HEIGHT), floats.
/// Signal `emacs-egui-raster-error' when SVG cannot be parsed.
#[defun]
fn svg_size<'e>(env: &'e Env, svg: Value<'e>) -> Result<Value<'e>> {
    guard(|| {
        let size = parse(env, svg, None)?.size();
        env.cons(size.width() as f64, size.height() as f64)
    })
}

/// What the last `emacs-egui-raster-svg' cost (internal): a plist of
/// :parse-ms :render-ms :encode-ms, and the PNG's :width and :height in
/// pixels.
#[defun(name = "-last-timing")]
fn last_timing(env: &Env) -> Result<Value<'_>> {
    guard(|| {
        let t = *LAST_TIMING.lock().unwrap_or_else(|e| e.into_inner());
        env.list((
            env.intern(":parse-ms")?,
            t.parse_ms,
            env.intern(":render-ms")?,
            t.render_ms,
            env.intern(":encode-ms")?,
            t.encode_ms,
            env.intern(":width")?,
            t.width as i64,
            env.intern(":height")?,
            t.height as i64,
        ))
    })
}

/// Panic on purpose (internal), to check that a panic reaches Lisp as a
/// `rust-panic' error naming it.
#[defun(name = "-test-panic")]
fn test_panic() -> Result<()> {
    guard(|| {
        let nothing: Option<()> = None;
        nothing.expect("deliberate panic inside a module function");
        Ok(())
    })
}
