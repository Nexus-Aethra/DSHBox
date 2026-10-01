//! Page-debugging surface driven through a headless browser.
//!
//! Four capabilities are exposed for external callers (CLI, plugins, an
//! agent): screenshot the page, query page elements, click an element, and
//! click a coordinate. All four are thin wrappers over the Chrome DevTools
//! Protocol rather than a reimplementation of layout or hit-testing.
//!
//! The browser is never bundled. One headless Chromium is launched per
//! container being debugged, against that container's authenticated loopback
//! URL, and kept alive between calls so a debugging session does not pay the
//! launch cost on every screenshot. Sessions are keyed by container id and
//! torn down by debug_close, by the daemon shutting down, or by the caller
//! re-opening the same container.

use std::collections::BTreeMap;
use std::path::PathBuf;

use box_browser::{resolve, validate_browser_path, BrowserCandidate, BrowserSession};
use box_foundation::read_config;
use serde_json::{json, Value};

use crate::dispatch::container_url;
use crate::state::DaemonState;

/// Hard cap on how many elements one query returns.
///
/// A broad selector (body *) on a chat UI can match thousands of nodes, and
/// every one of them is serialised across the RPC boundary. The cap keeps a
/// debugging call from producing a multi-megabyte reply.
const MAX_ELEMENTS: usize = 200;
/// Characters of text kept per element.
const MAX_TEXT: usize = 160;

/// Per-container browser sessions, keyed by container id.
pub(crate) type Sessions = BTreeMap<String, BrowserSession>;

fn lock_sessions(
    state: &DaemonState,
) -> Result<std::sync::MutexGuard<'_, Sessions>, String> {
    state
        .browser
        .lock()
        .map_err(|_| "debug session lock poisoned".to_owned())
}

/// The browser Box would use right now.
pub(crate) fn current_browser() -> Result<BrowserCandidate, String> {
    let config = read_config().map_err(|error| format!("cannot read config: {error}"))?;
    resolve(config.browser_path.as_deref())
}

/// Report which browser the debug surface will drive.
pub(crate) fn browser_status_rpc() -> Result<Value, String> {
    let configured = read_config()
        .ok()
        .and_then(|config| config.browser_path)
        .filter(|path| !path.trim().is_empty());
    match current_browser() {
        Ok(browser) => Ok(json!({
            "available": true,
            "kind": browser.kind.as_str(),
            "path": browser.path,
            "configured": browser.configured,
            "configuredPath": configured,
        })),
        Err(error) => Ok(json!({
            "available": false,
            "problem": error,
            "configuredPath": configured,
        })),
    }
}

fn write_config(config: &box_foundation::BoxConfig) -> Result<(), String> {
    box_foundation::write_config(config).map_err(|error| format!("cannot save config: {error}"))
}

/// Validate and persist a browser override supplied by an agent or the CLI.
///
/// The path is checked before it is written, so a typo is rejected where the
/// user can still see which value they typed, rather than the next time a
/// debug call mysteriously fails to find a browser.
pub(crate) fn set_browser_path_rpc(request: &Value) -> Result<Value, String> {
    let raw = request["path"].as_str().unwrap_or("").to_owned();
    let mut config = read_config()?;

    // An empty string clears the override rather than storing a path that
    // can never validate, so this doubles as 'go back to auto-detect'.
    if raw.trim().is_empty() {
        config.browser_path = None;
        write_config(&config)?;
        return Ok(json!({
            "configuredPath": Value::Null,
            "available": current_browser().is_ok(),
        }));
    }

    let path = validate_browser_path(&raw)?;
    let browser = resolve(Some(&path.display().to_string()))?;
    config.browser_path = Some(path.display().to_string());
    write_config(&config)?;
    Ok(json!({
        "configuredPath": browser.path,
        "kind": browser.kind.as_str(),
        "available": true,
    }))
}

/// Scratch profile directory for one container's browser session.
fn profile_dir(state: &DaemonState, id: &str) -> Result<PathBuf, String> {
    let root = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let runtime = root
        .runtime
        .as_ref()
        .ok_or_else(|| "no runtime directory configured".to_owned())?;
    Ok(runtime.join("debug-browser").join(id))
}

/// Launch (or replace) the headless session attached to a container.
pub(crate) fn open_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        return Err("debug_open requires a container id".to_owned());
    }
    let browser = current_browser()?;
    let url = container_url(state, &id)?;
    let profile = profile_dir(state, &id)?;

    let mut sessions = lock_sessions(state)?;
    // Re-opening replaces: a stale session is attached to a port that may
    // have been recycled by an unrelated process, which is worse than a
    // fresh launch.
    sessions.remove(&id);
    let session = BrowserSession::launch(&browser.path, &url, profile)?;
    let reply = json!({
        "id": id,
        "url": session.url(),
        "targetId": session.target_id(),
        "port": session.port(),
        "browser": browser.path,
        "kind": browser.kind.as_str(),
    });
    sessions.insert(id, session);
    Ok(reply)
}

/// Tear down one container's session.
pub(crate) fn close_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let mut sessions = lock_sessions(state)?;
    let existed = sessions.remove(&id).is_some();
    Ok(json!({ "id": id, "closed": existed }))
}

/// Run one CDP call against a container's session.
fn with_session<T>(
    state: &DaemonState,
    request: &Value,
    action: impl FnOnce(&mut BrowserSession) -> Result<T, String>,
) -> Result<T, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let mut sessions = lock_sessions(state)?;
    let session = sessions
        .get_mut(&id)
        .ok_or_else(|| format!("no debug session for {id}; call debug_open first"))?;
    action(session)
}

/// Evaluate an expression in the page and return its value.
fn evaluate(session: &mut BrowserSession, expression: &str) -> Result<Value, String> {
    let result = session.call(
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true
        }),
    )?;
    // Runtime.evaluate reports page-side failures in exceptionDetails with
    // a successful envelope, so a missing result is an error, not a null.
    if let Some(details) = result.get("exceptionDetails") {
        let text = details
            .get("exception")
            .and_then(|exception| exception.get("description"))
            .and_then(Value::as_str)
            .or_else(|| details.get("text").and_then(Value::as_str))
            .unwrap_or("page threw");
        return Err(format!("page evaluation failed: {text}"));
    }
    Ok(result
        .get("result")
        .and_then(|value| value.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Screenshot the page as a base64 image.
pub(crate) fn screenshot_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let format = request["format"].as_str().unwrap_or("png").to_owned();
    if !matches!(format.as_str(), "png" | "jpeg" | "webp") {
        return Err(format!("unsupported screenshot format {format}"));
    }
    let full_page = request["fullPage"].as_bool().unwrap_or(false);
    let mut params = json!({ "format": format, "captureBeyondViewport": full_page });
    if full_page {
        // captureBeyondViewport only produces a full-page image when the clip
        // is widened; without this the result is the viewport anyway.
        let metrics = with_session(state, request, |session| {
            session.call("Page.getLayoutMetrics", json!({}))
        })?;
        let content = &metrics["cssContentSize"];
        let width = content["width"].as_f64().unwrap_or(0.0);
        let height = content["height"].as_f64().unwrap_or(0.0);
        if width > 0.0 && height > 0.0 {
            params["clip"] = json!({
                "x": 0.0,
                "y": 0.0,
                "width": width,
                "height": height,
                "scale": 1
            });
        }
    }
    let data = with_session(state, request, |session| {
        let result = session.call("Page.captureScreenshot", params)?;
        result
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "Page.captureScreenshot returned no image".to_owned())
    })?;
    Ok(json!({ "format": format, "encoding": "base64", "data": data }))
}
/// Build the JS that describes every element matching a selector.
///
/// One multi-line literal rather than a chain of adjacent pieces: Rust does
/// not concatenate neighbouring string literals the way C does, so the
/// split-and-join form does not compile.
fn element_probe_js(selector: &str, limit: usize) -> String {
    // The selector is embedded as a JSON string literal, never pasted into
    // the expression: a caller-supplied selector is untrusted input, and
    // splicing it raw would turn element query into arbitrary script
    // execution inside the debugged page.
    let selector_literal = Value::String(selector.to_owned()).to_string();
    let text_cap = MAX_TEXT;
    format!(
        r#"JSON.stringify(
            Array.from(document.querySelectorAll({selector_literal}))
              .slice(0, {limit})
              .map(function (el) {{
            var r = el.getBoundingClientRect();
            return {{
              tag: (el.tagName || '').toLowerCase(),
              id: el.id || null,
              classes: (typeof el.className === 'string' && el.className)
                ? el.className.trim() || null
                : null,
              text: (el.textContent || '')
                .replace(/\s+/g, ' ')
                .trim()
                .slice(0, {text_cap}),
              visible: r.width > 0 && r.height > 0,
              x: r.x, y: r.y, width: r.width, height: r.height,
              centerX: r.x + r.width / 2,
              centerY: r.y + r.height / 2
            }};
          }}))"#
    )
}

fn parse_elements(value: Value) -> Result<Vec<Value>, String> {
    let text = value
        .as_str()
        .ok_or_else(|| "element query returned a non-string result".to_owned())?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(text).map_err(|error| format!("cannot parse element list: {error}"))
}

/// List the page elements matching a CSS selector.
pub(crate) fn query_elements_rpc(
    state: &DaemonState,
    request: &Value,
) -> Result<Value, String> {
    let selector = request["selector"].as_str().unwrap_or("body").to_owned();
    if selector.trim().is_empty() {
        return Err("selector must not be empty".to_owned());
    }
    let limit = request
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_ELEMENTS as u64)
        .clamp(1, MAX_ELEMENTS as u64) as usize;
    let expression = element_probe_js(&selector, limit);
    let raw = with_session(state, request, |session| evaluate(session, &expression))?;
    let elements = parse_elements(raw)?;
    Ok(json!({
        "selector": selector,
        "truncated": elements.len() >= limit,
        "count": elements.len(),
        "elements": elements
    }))
}

fn dispatch_click(session: &mut BrowserSession, x: f64, y: f64) -> Result<(), String> {
    // A click is a press plus a release; sending only one leaves the page
    // with a button held down.
    for kind in ["mousePressed", "mouseReleased"] {
        session.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": kind,
                "x": x,
                "y": y,
                "button": "left",
                "clickCount": 1
            }),
        )?;
    }
    Ok(())
}

/// Click the first element matching a selector.
pub(crate) fn click_element_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let selector = request["selector"].as_str().unwrap_or("").to_owned();
    if selector.trim().is_empty() {
        return Err("click_element requires a selector".to_owned());
    }
    let expression = element_probe_js(&selector, 1);
    let raw = with_session(state, request, |session| evaluate(session, &expression))?;
    let elements = parse_elements(raw)?;
    let target = elements
        .first()
        .ok_or_else(|| format!("no element matches {selector}"))?;
    let x = target["centerX"].as_f64().unwrap_or(0.0);
    let y = target["centerY"].as_f64().unwrap_or(0.0);
    with_session(state, request, |session| dispatch_click(session, x, y))?;
    Ok(json!({
        "selector": selector,
        "x": x,
        "y": y,
        "tag": target["tag"],
        "text": target["text"],
    }))
}

/// Click a point in viewport coordinates.
pub(crate) fn click_at_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let x = request["x"].as_f64().unwrap_or(-1.0);
    let y = request["y"].as_f64().unwrap_or(-1.0);
    if x < 0.0 || y < 0.0 {
        return Err("click_at requires non-negative x and y".to_owned());
    }
    with_session(state, request, |session| dispatch_click(session, x, y))?;
    Ok(json!({ "x": x, "y": y }))
}
