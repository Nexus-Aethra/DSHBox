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
    // Hit-test first: a click dispatched onto whatever covers the target is
    // not the click the caller asked for, and reporting it as one is how an
    // agent spends a turn wondering why the page did not change.
    let hit = with_session(state, request, |session| run_hit_test(session, x, y, Some(&selector)))?;
    with_session(state, request, |session| dispatch_click(session, x, y))?;
    Ok(click_response(
        json!({
            "selector": selector,
            "x": x,
            "y": y,
            "tag": target["tag"],
            "text": target["text"],
        }),
        hit,
    ))
}

/// Click a point in viewport coordinates.
pub(crate) fn click_at_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let x = request["x"].as_f64().unwrap_or(-1.0);
    let y = request["y"].as_f64().unwrap_or(-1.0);
    if x < 0.0 || y < 0.0 {
        return Err("click_at requires non-negative x and y".to_owned());
    }
    let hit = with_session(state, request, |session| run_hit_test(session, x, y, None))?;
    with_session(state, request, |session| dispatch_click(session, x, y))?;
    Ok(click_response(json!({ "x": x, "y": y }), hit))
}

/// Insert text into whatever currently has focus.
///
/// CDP's `Input.insertText` is the one input method that behaves the way a
/// person does: it dispatches the same beforeinput/input events a real key
/// would, so frameworks that track a controlled value (React, Vue, Svelte)
/// see the change. Setting `element.value` from `Runtime.evaluate` instead
/// updates the DOM and leaves the framework's state stale, which is why this
/// is its own verb rather than a call into the page.
pub(crate) fn type_text_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let text = request["text"].as_str().unwrap_or("").to_owned();
    if text.is_empty() {
        return Err("type_text requires a non-empty text".to_owned());
    }
    let length = text.chars().count();
    // Focus is read *before* inserting, and it decides whether the insert is
    // reported as having happened at all. `Input.insertText` succeeds into the
    // void when nothing editable holds focus, so a count alone is a claim the
    // caller cannot check -- and that silent no-op is exactly the failure this
    // verb exists to make visible.
    let focused = with_session(state, request, focused_field_tag)?;
    if focused.is_none() {
        return Ok(json!({ "inserted": 0, "focused": Value::Null }));
    }
    let owned = text.clone();
    with_session(state, request, move |session| {
        session.call("Input.insertText", json!({ "text": owned.clone() }))
    })?;
    Ok(json!({ "inserted": length, "focused": focused }))
}
/// A named key as CDP spells it, with the virtual key code a native event
/// carries. Only keys a driver actually needs are listed; anything else is
/// reported rather than guessed at.
struct KeySpec {
    key: &'static str,
    code: &'static str,
    key_code: u32,
    text: Option<&'static str>,
}

fn key_spec(name: &str) -> Option<KeySpec> {
    let spec = match name {
        "Enter" | "submit" | "return" => KeySpec { key: "Enter", code: "Enter", key_code: 13, text: Some("\r") },
        "Tab" => KeySpec { key: "Tab", code: "Tab", key_code: 9, text: Some("\t") },
        "Escape" | "esc" => KeySpec { key: "Escape", code: "Escape", key_code: 27, text: None },
        "Backspace" => KeySpec { key: "Backspace", code: "Backspace", key_code: 8, text: None },
        "Delete" | "del" => KeySpec { key: "Delete", code: "Delete", key_code: 46, text: None },
        "ArrowUp" | "up" => KeySpec { key: "ArrowUp", code: "ArrowUp", key_code: 38, text: None },
        "ArrowDown" | "down" => KeySpec { key: "ArrowDown", code: "ArrowDown", key_code: 40, text: None },
        "ArrowLeft" => KeySpec { key: "ArrowLeft", code: "ArrowLeft", key_code: 37, text: None },
        "ArrowRight" => KeySpec { key: "ArrowRight", code: "ArrowRight", key_code: 39, text: None },
        "Home" => KeySpec { key: "Home", code: "Home", key_code: 36, text: None },
        "End" => KeySpec { key: "End", code: "End", key_code: 35, text: None },
        "PageUp" => KeySpec { key: "PageUp", code: "PageUp", key_code: 33, text: None },
        "PageDown" => KeySpec { key: "PageDown", code: "PageDown", key_code: 34, text: None },
        "Shift" => KeySpec { key: "Shift", code: "ShiftLeft", key_code: 16, text: None },
        "Control" | "ctrl" => KeySpec { key: "Control", code: "ControlLeft", key_code: 17, text: None },
        "Alt" => KeySpec { key: "Alt", code: "AltLeft", key_code: 18, text: None },
        _ => return None,
    };
    Some(spec)
}

/// Press and release one named key.
///
/// Separate from `type_text` because a submit keystroke carries no text of
/// its own: an Enter that submits a form has to arrive as a real key event
/// for the form to hear about it.
pub(crate) fn press_key_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let name = request["key"].as_str().unwrap_or("").trim().to_owned();
    let spec = key_spec(&name).ok_or_else(|| format!("press_key does not know the key {name:?}"))?;
    with_session(state, request, |session| {
        let mut down = json!({
            "type": "keyDown",
            "key": spec.key,
            "code": spec.code,
            "windowsVirtualKeyCode": spec.key_code,
            "nativeVirtualKeyCode": spec.key_code,
        });
        // A printable key carries `text`, which is what actually produces the
        // character; modifiers must omit it or the page inserts garbage.
        if let Some(text) = spec.text {
            down["text"] = json!(text);
        }
        session.call("Input.dispatchKeyEvent", down)?;
        session.call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": spec.key,
                "code": spec.code,
                "windowsVirtualKeyCode": spec.key_code,
                "nativeVirtualKeyCode": spec.key_code,
            }),
        )
    })?;
    Ok(json!({ "key": spec.key }))
}

/// Default entries returned by `debug_page_text`.
const DEFAULT_PAGE_TEXT: usize = 200;
/// Hard cap on entries returned by `debug_page_text`.
///
/// Higher than MAX_ELEMENTS on purpose: the accessibility tree is already
/// filtered to things a person would act on, so the useful entries are a
/// small fraction of a page's DOM and a 200 ceiling would cut them off before
/// the caller saw the fold.
const MAX_PAGE_TEXT: usize = 1000;

/// Accessibility roles that are structure rather than content.
///
/// A chat UI's tree is mostly these. Keeping them would bury every actionable
/// line, and the caller's job is to find the actionable lines.
fn is_structural_noise(role: &str) -> bool {
    matches!(
        role,
        "none"
            | "presentation"
            | "generic"
            | "InlineTextBox"
            | "StaticText"
            | "LineBreak"
            | "Separator"
            | "ScrollBar"
            | "ScrollArea"
            | "RootWebArea"
    )
}

/// Roles that accept a pointer click.
///
/// Reported rather than assumed by the caller, so an agent can tell a control
/// it can press from a label it can only read. Case matters: roles arrive in
/// Chrome's canonical mixed case, and a lowercase compare would miss Button.
fn is_clickable_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "checkbox"
            | "radio"
            | "switch"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "option"
            | "tab"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "slider"
            | "spinbutton"
            | "treeitem"
    )
}

/// Viewport size and scroll offset, from one layout-metrics call.
struct Viewport {
    width: f64,
    height: f64,
}

fn viewport_metrics(session: &mut BrowserSession) -> Result<Viewport, String> {
    let metrics = session.call("Page.getLayoutMetrics", json!({}))?;
    let visual = &metrics["cssVisualViewport"];
    let layout = &metrics["cssLayoutViewport"];
    let number = |value: &Value, fallback: &Value| -> f64 {
        value
            .as_f64()
            .or_else(|| fallback.as_f64())
            .unwrap_or(0.0)
    };
    Ok(Viewport {
        width: number(&visual["clientWidth"], &layout["clientWidth"]),
        height: number(&visual["clientHeight"], &layout["clientHeight"]),
    })
}

/// Min/max box of a node, in the viewport coordinates a click uses.
///
/// The content quad carries eight points, and a transformed node can order
/// them in ways the usual top-left-first reading does not survive, so the
/// bounds come from every point rather than from indices.
fn backend_node_rect(
    session: &mut BrowserSession,
    backend_node_id: i64,
) -> Result<(f64, f64, f64, f64), String> {
    let box_model = session.call(
        "DOM.getBoxModel",
        json!({ "backendNodeId": backend_node_id }),
    )?;
    let points = box_model["model"]["content"]
        .as_array()
        .ok_or_else(|| "DOM.getBoxModel returned no content quad".to_owned())?;
    if points.is_empty() {
        return Err("DOM.getBoxModel returned an empty content quad".to_owned());
    }
    let coordinates: Vec<f64> = points.iter().filter_map(Value::as_f64).collect();
    let (min_x, min_y, width, height) = quad_bounds(&coordinates)
        .ok_or_else(|| "DOM.getBoxModel returned a degenerate content quad".to_owned())?;
    Ok((min_x, min_y, width, height))
}

/// Bounds of a box-model quad, as `(x, y, width, height)`.
///
/// CDP returns a quad as a **flat run of numbers** -- eight x,y pairs -- not
/// as a list of `{x, y}` points. Reading it as points does not fail: indexing
/// a number with `"x"` yields null, every coordinate becomes 0.0, and the
/// whole page reports as one degenerate zero-sized box at the origin. Bounds
/// therefore come from consecutive pairs, which is also what survives a
/// transformed node whose vertices are not in the usual corner order.
///
/// The numbers are already viewport-relative, which is what makes them
/// directly usable as click coordinates and comparable against a viewport.
fn quad_bounds(coordinates: &[f64]) -> Option<(f64, f64, f64, f64)> {
    let pairs = coordinates.len() / 2;
    if pairs < 2 {
        return None;
    }
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;
    for pair in coordinates.chunks_exact(2) {
        let (x, y) = (pair[0], pair[1]);
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    Some((min_x, min_y, max_x - min_x, max_y - min_y))
}
/// Render the page as text from its accessibility tree.
///
/// A screenshot shows what the page looks like and a selector query answers
/// one expression, but the question an agent actually has is what is on the
/// page, and the accessibility tree is the honest source: it carries the role
/// and the accessible name, which is what the page claims to *be*, rather
/// than a tag and some innerText.
pub(crate) fn page_text_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let limit = request
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_PAGE_TEXT)
        .clamp(1, MAX_PAGE_TEXT);
    let role_filter = request
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);

    with_session(state, request, |session| {
        // An unavailable Accessibility domain is an error, never an empty
        // list: an empty list reads as a blank page and sends the caller
        // looking for content that is in fact right there.
        let tree = session
            .call("Accessibility.getFullAXTree", json!({}))
            .map_err(|error| {
                format!("accessibility tree is unavailable on this target: {error}")
            })?;
        let viewport = viewport_metrics(session)?;

        let mut elements: Vec<Value> = Vec::new();
        let mut below_fold = 0usize;
        let mut truncated = false;
        for node in tree["nodes"].as_array().into_iter().flatten() {
            if node["ignored"].as_bool().unwrap_or(false) {
                continue;
            }
            let role = node["role"]["value"].as_str().unwrap_or("");
            if role.is_empty() || is_structural_noise(role) {
                continue;
            }
            if let Some(wanted) = role_filter.as_deref() {
                if !role.to_ascii_lowercase().contains(wanted) {
                    continue;
                }
            }
            let name = node["name"]["value"].as_str().unwrap_or("");
            let value = node["value"]["value"].as_str().unwrap_or("");
            if name.is_empty() && value.is_empty() {
                continue;
            }
            if elements.len() >= limit {
                truncated = true;
                break;
            }

            // The rect keys are seeded null so every entry has the same shape
            // and a caller can read them without probing for their presence.
            let mut entry = json!({
                "role": role,
                "name": name,
                "clickable": is_clickable_role(role),
                "x": Value::Null,
                "y": Value::Null,
                "width": Value::Null,
                "height": Value::Null,
                "inViewport": Value::Null,
            });
            if !value.is_empty() {
                entry["value"] = json!(value);
            }
            // A node with no box is laid out as display:none and has no place
            // to be clicked. The rect keys stay present and null rather than
            // being omitted or zeroed: omitting makes the array ragged for every
            // consumer, and zeroing would publish the document origin as a
            // clickable point, which is a lie a caller cannot detect.
            if let Some(id) = node["backendDOMNodeId"].as_i64() {
                if let Ok((x, y, width, height)) = backend_node_rect(session, id) {
                    let in_viewport = width > 0.0
                        && height > 0.0
                        && x < viewport.width
                        && y < viewport.height
                        && x + width > 0.0
                        && y + height > 0.0;
                    if !in_viewport {
                        below_fold += 1;
                    }
                    entry["x"] = json!(((x + width / 2.0) * 10.0).round() / 10.0);
                    entry["y"] = json!(((y + height / 2.0) * 10.0).round() / 10.0);
                    entry["width"] = json!(width.round());
                    entry["height"] = json!(height.round());
                    entry["inViewport"] = json!(in_viewport);
                }
            }
            elements.push(entry);
        }

        Ok(json!({
            "count": elements.len(),
            "truncated": truncated,
            "belowFold": below_fold,
            "elements": elements
        }))
    })
}

/// JS that reports scroll extent and, when given a target, moves there.
///
/// `to` is interpolated as a number literal, never as script text, so a
/// caller cannot turn a scroll into code execution inside the debugged page.
/// One multi-line literal: Rust does not concatenate neighbouring string
/// literals the way C does.
fn scroll_probe_js(to: Option<f64>) -> String {
    let target = match to {
        Some(value) => value.to_string(),
        None => "null".to_owned(),
    };
    format!(
        r#"JSON.stringify((function () {{
          var el = document.scrollingElement || document.documentElement;
          if (!el) return {{ scrollTop: 0, scrollHeight: 0, viewportHeight: 0, screensBelow: 0, moved: false }};
          var wanted = {target};
          var moved = false;
          if (wanted !== null) {{
            el.scrollTop = wanted;
            moved = true;
          }}
          var viewportHeight = window.innerHeight || el.clientHeight || 0;
          var remaining = el.scrollHeight - (el.scrollTop + viewportHeight);
          return {{
            scrollTop: el.scrollTop,
            scrollHeight: el.scrollHeight,
            viewportHeight: viewportHeight,
            screensBelow: remaining > 0 ? Math.ceil(remaining / viewportHeight) : 0,
            moved: moved
          }};
        }})())"#
    )
}

/// Report how far the page extends, and optionally scroll to an offset.
///
/// Without this a caller has no way to know a form is three screens tall,
/// which is how a required field gets skipped: the first screen looks
/// complete, and only submitting reveals what was underneath.
pub(crate) fn scroll_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let to = request.get("to").and_then(Value::as_f64);
    let expression = scroll_probe_js(to);
    let raw = with_session(state, request, |session| evaluate(session, &expression))?;
    let text = raw
        .as_str()
        .ok_or_else(|| "scroll probe returned a non-string result".to_owned())?;
    let measured: Value = serde_json::from_str(text)
        .map_err(|error| format!("cannot parse scroll probe result: {error}"))?;
    Ok(json!({
        "scrollTop": measured["scrollTop"].clone(),
        "scrollHeight": measured["scrollHeight"].clone(),
        "viewportHeight": measured["viewportHeight"].clone(),
        "screensBelow": measured["screensBelow"].clone(),
        "moved": measured["moved"].clone(),
    }))
}

/// What was actually under a click, and whether it was the intended target.
struct HitTest {
    landed: bool,
    hit_tag: Option<String>,
    hit_text: Option<String>,
    occluded_by: Option<String>,
}

/// JS that reports what `document.elementFromPoint` finds at a point.
///
/// With a selector, the hit is also compared against the intended node. A
/// click counts as landed when the hit *is* the target, sits inside it, or
/// contains it: a button's inner span and the button itself are the same
/// click, while an overlay on top of it is a different one.
fn hit_test_js(x: f64, y: f64, selector: Option<&str>) -> String {
    let target_check = match selector {
        Some(_) => {
            "var target = document.querySelector(SELECTOR);"
                .to_owned()
        }
        None => "var target = null;".to_owned(),
    };
    let comparison = match selector {
        Some(_) => {
            r#"out.landed = !!hit && !!target
                 && (target === hit || target.contains(hit) || hit.contains(target));"#
                .to_owned()
        }
        None => "out.landed = !!hit;".to_owned(),
    };
    let selector_literal = match selector {
        Some(selector) => Value::String(selector.to_owned()).to_string(),
        None => "null".to_owned(),
    };
    let text_cap = MAX_TEXT;
    format!(
        r#"JSON.stringify((function () {{
          var x = {x}, y = {y};
          var out = {{ landed: false, hitTag: null, hitText: null, occludedBy: null }};
          var hit = document.elementFromPoint(x, y);
          if (hit) {{
            out.hitTag = (hit.tagName || '').toLowerCase();
            out.hitText = (hit.textContent || '')
              .replace(/\s+/g, ' ')
              .trim()
              .slice(0, {text_cap});
          }}
          {target_check}
          {comparison}
          if (!out.landed) {{
            out.occludedBy = out.hitTag
              ? ('the point is over <' + out.hitTag + '>' + (out.hitText ? ' "' + out.hitText + '"' : ''))
              : 'nothing is at this point (it is outside the page or fully covered)';
          }}
          return out;
        }})())"#
    )
    .replace("SELECTOR", &selector_literal)
}

fn run_hit_test(
    session: &mut BrowserSession,
    x: f64,
    y: f64,
    selector: Option<&str>,
) -> Result<HitTest, String> {
    let expression = hit_test_js(x, y, selector);
    let raw = evaluate(session, &expression)?;
    let text = raw
        .as_str()
        .ok_or_else(|| "hit test returned a non-string result".to_owned())?;
    let value: Value = serde_json::from_str(text)
        .map_err(|error| format!("cannot parse hit test result: {error}"))?;
    Ok(HitTest {
        landed: value["landed"].as_bool().unwrap_or(false),
        hit_tag: value["hitTag"]
            .as_str()
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned),
        hit_text: value["hitText"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        occluded_by: value["occludedBy"]
            .as_str()
            .filter(|reason| !reason.is_empty())
            .map(str::to_owned),
    })
}

/// Merge a hit test into a click response, keeping the verdict adjacent to
/// the evidence that produced it.
fn click_response(base: Value, hit: HitTest) -> Value {
    let mut response = base;
    response["landed"] = json!(hit.landed);
    response["hitTag"] = json!(hit.hit_tag);
    response["hitText"] = json!(hit.hit_text);
    if !hit.landed {
        response["occludedBy"] = json!(hit.occluded_by);
    }
    response
}

/// JS that names the focused editable field, or null when there is none.
///
/// "Editable" is the page's own answer rather than a list of tags: a disabled
/// or read-only input reports false for isContentEditable and takes focus in
/// some frameworks, so it is excluded explicitly.
const FOCUS_PROBE_JS: &str = r#"(function () {
  var el = document.activeElement;
  if (!el || el === document.body) return null;
  var tag = (el.tagName || '').toLowerCase();
  var editable = tag === 'input' || tag === 'textarea' || el.isContentEditable === true;
  if (!editable) return null;
  if (el.disabled === true || el.readOnly === true) return null;
  if (tag === 'input' && (el.type === 'hidden' || el.type === 'checkbox' || el.type === 'radio')) {
    return null;
  }
  return tag;
})()"#;

/// Tag of the focused editable field, or `None` when nothing editable has it.
fn focused_field_tag(session: &mut BrowserSession) -> Result<Option<String>, String> {
    let value = evaluate(session, FOCUS_PROBE_JS)?;
    Ok(value
        .as_str()
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_noise_is_filtered_by_exact_role() {
        for role in ["none", "presentation", "generic", "StaticText", "ScrollBar"] {
            assert!(is_structural_noise(role), "{role} should be noise");
        }
        // Substring matching would swallow real content: "GenericButton" and
        // "presentationCard" are not the roles being filtered.
        for role in ["Button", "StaticLabel", "ScrollableRegion", "Generic"] {
            assert!(!is_structural_noise(role), "{role} must survive");
        }
    }

    #[test]
    fn clickable_roles_match_chromes_canonical_casing() {
        for role in ["button", "link", "textbox", "checkbox", "combobox", "tab"] {
            assert!(is_clickable_role(role), "{role} should be clickable");
        }
        // Chrome emits "Button"; a lowercase compare would report every
        // button on the page as unclickable.
        assert!(!is_clickable_role("Button"), "canonical case is not matched");
        for role in ["heading", "paragraph", "image", "list"] {
            assert!(!is_clickable_role(role), "{role} must not be clickable");
        }
    }

    #[test]
    fn key_spec_resolves_aliases_to_one_canonical_spelling() {
        for alias in ["Enter", "submit", "return", "  Enter  "] {
            let spec = key_spec(alias.trim()).expect("Enter should resolve");
            assert_eq!(spec.key, "Enter");
        }
        assert_eq!(key_spec("esc").map(|spec| spec.key), Some("Escape"));
        assert_eq!(key_spec("up").map(|spec| spec.key), Some("ArrowUp"));
        // Submitting is Enter's only textual payload; a modifier that carried
        // one would insert a control character into the field.
        assert_eq!(key_spec("Enter").and_then(|spec| spec.text), Some("\r"));
        assert_eq!(key_spec("Shift").and_then(|spec| spec.text), None);
    }

    #[test]
    fn key_spec_refuses_names_it_cannot_honour() {
        // A guessed key code is worse than no key: it dispatches an event the
        // page did not ask for and reports success.
        for name in ["F13", "nope", "", "Ctrl+S"] {
            assert!(key_spec(name).is_none(), "{name:?} must not resolve");
        }
    }

    #[test]
    fn quad_bounds_reads_the_flat_number_run_cdp_actually_returns() {
        // Captured from headless Edge, content quad of a button at viewport
        // y=200 on a page scrolled to 1000. Read as `{x, y}` points this
        // yields eight zeroes and every element reports as 0x0 at the origin.
        let quad = [48.0, 203.0, 232.0, 203.0, 232.0, 237.0, 48.0, 237.0];
        let (x, y, width, height) = quad_bounds(&quad).expect("quad should parse");
        assert_eq!((x, y), (48.0, 203.0));
        assert_eq!((width, height), (184.0, 34.0));
    }

    #[test]
    fn quad_bounds_survives_unordered_and_degenerate_input() {
        // Vertices are not guaranteed to arrive corner-first.
        let (x, y, width, height) = quad_bounds(&[232.0, 237.0, 48.0, 203.0, 232.0, 203.0])
            .expect("three points still bound a box");
        assert_eq!((x, y), (48.0, 203.0));
        assert_eq!((width, height), (184.0, 34.0));
        // A single point is a point, not a box; returning None keeps the
        // caller from reporting a plausible-looking zero size.
        assert_eq!(quad_bounds(&[10.0, 10.0]), None);
        assert_eq!(quad_bounds(&[]), None);
        // A non-finite coordinate would poison the min/max and poison every
        // click derived from it.
        assert_eq!(quad_bounds(&[f64::NAN, 0.0, 1.0, 1.0, 2.0, 2.0, 3.0, 3.0]), None);
    }
    #[test]
    fn hit_test_js_never_splices_a_selector_into_script() {
        // A selector is untrusted input. Pasted raw it would turn a hit test
        // into arbitrary script execution inside the debugged page.
        let expression = hit_test_js(1.0, 2.0, Some("img[src='x']; alert(1)"));
        // The selector travels as a JSON string literal, so its semicolon is
        // inside quotes and cannot terminate the statement it sits in.
        assert!(expression.contains(r#""img[src='x']; alert(1)""#));
        // And nothing from the input became executable script.
        assert!(!expression.contains("= alert(1)"));
        // A click with no intended element must not smuggle a selector in.
        assert!(!hit_test_js(1.0, 2.0, None).contains("querySelector(\\\""));
    }

}
