//! mado-web — browser-side mado renderer skeleton.
//!
//! This is the **wire-foundation** slice: the wasm crate connects
//! to a `tear-ws-bridge` over WebSocket, sends typed CBOR
//! [`Request`]s, and decodes typed [`Response`]s — proving the
//! full path from a browser back to the tear-daemon works.
//!
//! What this slice DOES:
//! - Opens a WebSocket to a configurable URL (default
//!   `ws://localhost:8181/`).
//! - Sends a `Request::ListSessions` once on connect.
//! - Decodes the daemon's `Response::Sessions(...)` and renders
//!   the list into a `<ul id="sessions">` in the host page.
//! - Subscribes to a chosen pane's byte stream and appends the
//!   raw bytes (as UTF-8) to a `<pre id="pane-stream">` —
//!   smoke-test of the push-mode subscription path.
//!
//! What this slice DOES NOT do (yet):
//! - Render a cell grid in WebGPU. The terminal-rendering layer
//!   is a separate slice — for now we just stream raw bytes so
//!   the operator can see the wire works end-to-end.
//! - Handle keyboard input → send_keys. Read-only attach today.
//!
//! The next slices add a WebGPU canvas + glyph atlas + cell
//! pipeline (~1 week of work each).

#![allow(clippy::unused_unit)] // wasm-bindgen leaves () returns occasionally
#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use ciborium::{de::from_reader, ser::into_writer};
use serde::Serialize;
use tear_types::wire::{Request, Response, MAX_FRAME_BYTES};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, MessageEvent, WebSocket};

#[wasm_bindgen]
pub fn init_panic_hook() {
    console_error_panic_hook::set_once();
}

/// Entry point — called from the bootstrap JS. `ws_url` is the
/// tear-ws-bridge listen address (e.g. `"ws://localhost:8181/"`).
/// `pane_hex_to_subscribe` is optional; when Some, the wasm
/// client subscribes to that pane's byte stream after the
/// initial ListSessions probe.
#[wasm_bindgen]
pub fn connect(ws_url: &str, pane_hex_to_subscribe: Option<String>) -> Result<(), JsValue> {
    let ws = WebSocket::new(ws_url)?;
    ws.set_binary_type(BinaryType::Arraybuffer);

    let ws_rc: Rc<WebSocket> = Rc::new(ws.clone());
    let subscribe_target = Rc::new(RefCell::new(pane_hex_to_subscribe));

    // ── onopen: kick off with a ListSessions probe ─────────
    {
        let ws_for_onopen = ws_rc.clone();
        let onopen = Closure::wrap(Box::new(move |_event: web_sys::Event| {
            log("ws open — sending Request::ListSessions");
            if let Err(e) = send_request(&ws_for_onopen, &Request::ListSessions) {
                log(&format!("send error: {e:?}"));
            }
        }) as Box<dyn FnMut(_)>);
        ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        onopen.forget();
    }

    // ── onmessage: decode each binary frame ────────────────
    {
        let ws_for_onmsg = ws_rc.clone();
        let target_for_onmsg = subscribe_target.clone();
        let onmessage = Closure::wrap(Box::new(move |event: MessageEvent| {
            if let Ok(buf) = event.data().dyn_into::<js_sys::ArrayBuffer>() {
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                handle_frame(&bytes, &ws_for_onmsg, &target_for_onmsg);
            }
        }) as Box<dyn FnMut(_)>);
        ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();
    }

    // ── onerror / onclose: log to the dev console ─────────
    {
        let onerror = Closure::wrap(Box::new(move |event: web_sys::ErrorEvent| {
            log(&format!("ws error: {}", event.message()));
        }) as Box<dyn FnMut(_)>);
        ws.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        onerror.forget();
    }
    {
        let onclose = Closure::wrap(Box::new(move |event: web_sys::CloseEvent| {
            log(&format!("ws closed: code={} reason={}", event.code(), event.reason()));
        }) as Box<dyn FnMut(_)>);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();
    }

    Ok(())
}

/// Handle one decoded WS binary frame. Each frame is a complete
/// length-prefixed CBOR Response — we strip the length prefix
/// then deserialise.
fn handle_frame(
    bytes: &[u8],
    ws: &Rc<WebSocket>,
    subscribe_target: &Rc<RefCell<Option<String>>>,
) {
    if bytes.len() < 4 {
        log("frame too short for length prefix");
        return;
    }
    let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if len > MAX_FRAME_BYTES || bytes.len() < 4 + len {
        log(&format!("frame length mismatch: declared={} actual={}", len, bytes.len() - 4));
        return;
    }
    let payload = &bytes[4..4 + len];
    let resp: Response = match from_reader(payload) {
        Ok(r) => r,
        Err(e) => {
            log(&format!("decode error: {e}"));
            return;
        }
    };
    match resp {
        Response::Sessions(sessions) => {
            render_sessions(&sessions);
            // If the operator pre-declared a pane to attach,
            // promote this connection to a Subscribe.
            if let Some(pane_hex) = subscribe_target.borrow_mut().take() {
                if let Ok(pane_id) = pane_hex.parse::<tear_types::PaneId>() {
                    log(&format!("subscribing to pane {pane_id}"));
                    let _ = send_request(ws, &Request::Subscribe(pane_id));
                }
            }
        }
        Response::Ok => {
            log("Subscribe acknowledged — streaming bytes");
        }
        Response::PaneBytes(b) => {
            append_pane_stream(&b);
        }
        Response::PaneClosed(_) => {
            log("pane closed by daemon");
        }
        Response::Err(we) => {
            log(&format!("wire error: {we:?}"));
        }
        other => {
            log(&format!("unhandled response variant: {other:?}"));
        }
    }
}

fn send_request<T: Serialize>(ws: &WebSocket, req: &T) -> Result<(), JsValue> {
    let mut payload = Vec::new();
    into_writer(req, &mut payload).map_err(|e| JsValue::from_str(&format!("cbor: {e}")))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| JsValue::from_str("frame too large"))?;
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&len.to_be_bytes());
    framed.extend_from_slice(&payload);
    ws.send_with_u8_array(&framed)
}

fn render_sessions(sessions: &[tear_types::TearSession]) {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let document = match window.document() {
        Some(d) => d,
        None => return,
    };
    let ul = match document.get_element_by_id("sessions") {
        Some(e) => e,
        None => return,
    };
    ul.set_inner_html("");
    for s in sessions {
        let li = document.create_element("li").ok();
        if let Some(li) = li {
            li.set_text_content(Some(&format!(
                "{} {}  windows={}  panes={}  source={}",
                s.id,
                s.name,
                s.windows.len(),
                s.panes.len(),
                s.source.label(),
            )));
            let _ = ul.append_child(&li);
        }
    }
    if sessions.is_empty() {
        let li = document.create_element("li").ok();
        if let Some(li) = li {
            li.set_text_content(Some("(no sessions)"));
            let _ = ul.append_child(&li);
        }
    }
}

fn append_pane_stream(bytes: &[u8]) {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let document = match window.document() {
        Some(d) => d,
        None => return,
    };
    let pre = match document.get_element_by_id("pane-stream") {
        Some(e) => e,
        None => return,
    };
    let text = String::from_utf8_lossy(bytes);
    let existing = pre.text_content().unwrap_or_default();
    pre.set_text_content(Some(&(existing + &text)));
}

fn log(msg: &str) {
    web_sys::console::log_1(&JsValue::from_str(msg));
}
