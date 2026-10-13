//! Events to Elisp. Each event is one line, `(ID ACTION . PAYLOAD)`, written
//! by a thread of its own into the pipe process Elisp opened with
//! `PREFIX-open-channel`, so a full pipe never stalls a frame.

use std::sync::{mpsc, Mutex};

use crate::{convert, lock, render};

static CHANNEL: Mutex<Option<mpsc::Sender<String>>> = Mutex::new(None);

pub(crate) fn set_channel(tx: mpsc::Sender<String>) {
    *lock(&CHANNEL) = Some(tx);
}

/// Queue one event line. ACTION is a Lisp symbol as printed; PAYLOAD is the
/// body of a plist in Lisp syntax, possibly empty.
pub(crate) fn emit(id: u64, action: &str, payload: &str) {
    let line = if payload.is_empty() {
        format!("({id} {action})\n")
    } else {
        format!("({id} {action} {payload})\n")
    };
    if let Some(tx) = lock(&CHANNEL).as_ref() {
        let _ = tx.send(line);
    }
}

/// The SDK's message sink: `emacs_post_message` called by an application
/// from inside `update`, on the render thread of the view it draws. A
/// message from any other thread names no view and is dropped.
pub(crate) fn app_message(action: &str, payload: serde_json::Value) {
    if let Some(id) = render::current_id() {
        emit(
            id,
            &convert::lisp_symbol(action),
            &convert::payload_body(&payload),
        );
    }
}
