//! Lisp values to JSON and back, at the module boundary.
//!
//! A pushed state crosses as a [`serde_json::Value`], the type the web
//! backend parses its JSON into, so an application deserializes the same
//! value on both backends. The mapping is the one the Elisp backend's web
//! encoder uses:
//!
//! | Lisp | JSON |
//! |---|---|
//! | a plist (a list whose first element is a keyword) | an object; each key is the keyword's name without the colon |
//! | any other proper list, a vector | an array |
//! | a string | a string |
//! | an integer, a float | a number |
//! | `t` | `true` |
//! | `nil` | `null` |
//! | `:json-false` | `false` |
//! | any other keyword or symbol | its name as a string (a keyword without the colon) |
//!
//! Anything else (a dotted list, a plist of odd length or with a key that is
//! not a keyword, a bignum, a NaN, a buffer, ...) is an error naming where in
//! the value it was, as `sections[0].rows[2].label`.
//!
//! JSON to Lisp, for event payloads and `-info`: an object is a plist with
//! keyword keys, an array a list, `true` is `t`, `false` and `null` are
//! `nil`; as `json-parse-string` with `:object-type 'plist :array-type 'list
//! :null-object nil :false-object nil` reads it.

use emacs::{Env, IntoLisp, Result, Value};
use serde_json::{Map, Number, Value as Json};

fn is(env: &Env, predicate: &str, v: Value) -> Result<bool> {
    Ok(env.call(predicate, (v,))?.is_not_nil())
}

fn symbol_name(env: &Env, v: Value) -> Result<String> {
    env.call("symbol-name", (v,))?.into_rust::<String>()
}

fn at(path: &str) -> &str {
    if path.is_empty() {
        "the top level"
    } else {
        path
    }
}

/// Convert V to JSON; an error names the part of V that has no mapping.
pub(crate) fn lisp_to_json(env: &Env, v: Value) -> Result<Json> {
    let mut path = String::new();
    convert(env, v, &mut path)
}

fn convert(env: &Env, v: Value, path: &mut String) -> Result<Json> {
    if !v.is_not_nil() {
        return Ok(Json::Null);
    }
    if v == env.intern("t")? {
        return Ok(Json::Bool(true));
    }
    if is(env, "integerp", v)? {
        return match v.into_rust::<i64>() {
            Ok(n) => Ok(Json::Number(n.into())),
            Err(_) => Err(anyhow::anyhow!("an integer too large for JSON at {}", at(path)).into()),
        };
    }
    if is(env, "floatp", v)? {
        let x = v.into_rust::<f64>()?;
        return Number::from_f64(x).map(Json::Number).ok_or_else(|| {
            anyhow::anyhow!("the float {x} has no JSON form, at {}", at(path)).into()
        });
    }
    if is(env, "stringp", v)? {
        return Ok(Json::String(v.into_rust::<String>()?));
    }
    if is(env, "keywordp", v)? {
        let name = symbol_name(env, v)?;
        if name == ":json-false" {
            return Ok(Json::Bool(false));
        }
        return Ok(Json::String(name.trim_start_matches(':').to_string()));
    }
    if is(env, "symbolp", v)? {
        return Ok(Json::String(symbol_name(env, v)?));
    }
    if is(env, "vectorp", v)? {
        let n = env.call("length", (v,))?.into_rust::<i64>()?;
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let item = env.call("aref", (v, i))?;
            let len = path.len();
            path.push_str(&format!("[{i}]"));
            out.push(convert(env, item, path)?);
            path.truncate(len);
        }
        return Ok(Json::Array(out));
    }
    if is(env, "consp", v)? {
        if !env.call("proper-list-p", (v,))?.is_not_nil() {
            return Err(anyhow::anyhow!("a dotted list at {}", at(path)).into());
        }
        let first: Value = v.car()?;
        if is(env, "keywordp", first)? {
            return plist(env, v, path);
        }
        let mut out = Vec::new();
        let mut rest = v;
        let mut i = 0;
        while rest.is_not_nil() {
            let item: Value = rest.car()?;
            let len = path.len();
            path.push_str(&format!("[{i}]"));
            out.push(convert(env, item, path)?);
            path.truncate(len);
            rest = rest.cdr()?;
            i += 1;
        }
        return Ok(Json::Array(out));
    }
    let kind = symbol_name(env, env.call("type-of", (v,))?)?;
    Err(anyhow::anyhow!("a {kind} has no JSON form, at {}", at(path)).into())
}

fn plist(env: &Env, v: Value, path: &mut String) -> Result<Json> {
    let mut out = Map::new();
    let mut rest = v;
    while rest.is_not_nil() {
        let key: Value = rest.car()?;
        if !is(env, "keywordp", key)? {
            let printed = env.call("prin1-to-string", (key,))?.into_rust::<String>()?;
            return Err(anyhow::anyhow!(
                "a plist key that is not a keyword, {printed}, at {}",
                at(path)
            )
            .into());
        }
        let name = symbol_name(env, key)?;
        let name = name.trim_start_matches(':').to_string();
        let tail: Value = rest.cdr()?;
        if !tail.is_not_nil() {
            return Err(anyhow::anyhow!(
                "a plist of odd length: :{name} has no value, at {}",
                at(path)
            )
            .into());
        }
        let len = path.len();
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(&name);
        let value = convert(env, tail.car()?, path)?;
        path.truncate(len);
        out.insert(name, value);
        rest = tail.cdr()?;
    }
    Ok(Json::Object(out))
}

/// Build the Lisp value for V.
pub(crate) fn json_to_lisp<'e>(env: &'e Env, v: &Json) -> Result<Value<'e>> {
    match v {
        Json::Null | Json::Bool(false) => ().into_lisp(env),
        Json::Bool(true) => env.intern("t"),
        Json::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i.into_lisp(env),
            (None, Some(f)) => f.into_lisp(env),
            (None, None) => ().into_lisp(env),
        },
        Json::String(s) => s.as_str().into_lisp(env),
        Json::Array(items) => {
            let vals = items
                .iter()
                .map(|item| json_to_lisp(env, item))
                .collect::<Result<Vec<_>>>()?;
            env.call("list", &vals[..])
        }
        Json::Object(map) => {
            let mut vals = Vec::with_capacity(map.len() * 2);
            for (k, item) in map {
                vals.push(env.intern(&format!(":{k}"))?);
                vals.push(json_to_lisp(env, item)?);
            }
            env.call("list", &vals[..])
        }
    }
}

/// A Lisp string literal for `s`, readable by `read`.
pub(crate) fn lisp_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{:o}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A Lisp symbol for `name`, escaped so `read` returns a symbol of that name.
pub(crate) fn lisp_symbol(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || "-_+*/<>=!?$%&~^:.".contains(c) {
            out.push(c);
        } else {
            out.push('\\');
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push_str("##");
    }
    out
}

/// V printed as Lisp, by the JSON to Lisp mapping.
pub(crate) fn json_printed(v: &Json) -> String {
    match v {
        Json::Null | Json::Bool(false) => "nil".to_string(),
        Json::Bool(true) => "t".to_string(),
        Json::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (None, Some(u), _) => u.to_string(),
            // Debug keeps a decimal point or an exponent, so `read` sees a float.
            (None, None, Some(f)) => format!("{f:?}"),
            _ => "nil".to_string(),
        },
        Json::String(s) => lisp_string(s),
        Json::Array(items) => {
            let inner: Vec<String> = items.iter().map(json_printed).collect();
            format!("({})", inner.join(" "))
        }
        Json::Object(map) => format!("({})", plist_body(map)),
    }
}

fn plist_body(map: &Map<String, Json>) -> String {
    map.iter()
        .map(|(k, v)| format!(":{} {}", lisp_symbol(k), json_printed(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An event payload printed as the body of a plist: an object's pairs, nothing
/// for `null`, and `:value V` for anything else.
pub(crate) fn payload_body(v: &Json) -> String {
    match v {
        Json::Null => String::new(),
        Json::Object(map) => plist_body(map),
        other => format!(":value {}", json_printed(other)),
    }
}
