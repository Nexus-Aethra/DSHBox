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
    let outcome = action(session);
    // A browser that died leaves a session whose socket is closed. Every later
    // call then fails with a transport error about a dead connection, which
    // reads as a broken page rather than a browser that is no longer there --
    // and re-opening does not help, because the stale session is still in the
    // map. Dropping it here turns the next call into the "call debug_open
    // first" message that actually says what to do.
    if let Err(error) = &outcome {
        if is_dead_session_error(error) {
            sessions.remove(&id);
        }
    }
    outcome
}

/// Whether an error means the browser behind a session is gone.
///
/// Matched on the transport failures a closed socket produces rather than on
/// any error containing a word: a page that throws must keep its session.
fn is_dead_session_error(error: &str) -> bool {
    if error.contains("IO error") || error.contains("os error 10054") {
        return true;
    }
    [
        "connection reset",
        "connection closed",
        "connection refused",
        "not connected",
        "broken pipe",
        "WebSocket",
    ]
    .iter()
    .any(|marker| error.contains(marker))
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
    // Move first. A great deal of interface opens on hover rather than on
    // click -- a menu revealing its submenu, a card expanding, a tooltip
    // carrying the only instructions on screen -- and a press delivered with
    // no preceding move never triggers it. The press then lands on a menu
    // that is not open, so the caller clicks a submenu item that does not
    // exist and is told the click landed.
    session.call(
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseMoved",
            "x": x,
            "y": y,
            "button": "none",
            "buttons": 0
        }),
    )?;
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

/// Scroll a node into the middle of the window and return its rect afterwards.
///
/// Clicking the centre of a control that is half past the bottom edge puts the
/// point on the boundary: the click reports landing and nothing receives it. A
/// control a listing named but a click cannot focus is indistinguishable from
/// one that does not exist, so it is brought fully into view first.
fn scroll_node_into_view(
    session: &mut BrowserSession,
    backend_node_id: i64,
) -> Result<(f64, f64, f64, f64), String> {
    let resolved = session.call("DOM.resolveNode", json!({ "backendNodeId": backend_node_id }))?;
    let Some(object) = resolved["object"]["objectId"].as_str().map(str::to_owned) else {
        return backend_node_rect(session, backend_node_id);
    };
    session.call(
        "Runtime.callFunctionOn",
        json!({
            "objectId": object,
            "functionDeclaration":
                "function () { this.scrollIntoView({ block: 'center', inline: 'nearest' }); }",
            "returnByValue": true,
        }),
    )?;
    backend_node_rect(session, backend_node_id)
}
/// Find a visible element by its rendered text, without the accessibility tree.
///
/// Some menus are simply not in the accessibility tree. A provider picker
/// rendered in a portal with its own accessibility context is one: it opens,
/// it is on screen, a screenshot shows every item in it, and a listing of the
/// accessibility tree returns nothing at all. An agent told "nothing named
/// that matches" would conclude the control is not there.
///
/// So the search falls back to the document: walk the rendered text, keep what
/// is visible and sized, and return the smallest element carrying the name --
/// the smallest, because a container and its label both contain the same
/// words and clicking the container can miss the control inside it.
fn dom_text_probe_js(name: &str) -> String {
    let needle = serde_json::to_string(name).unwrap_or_else(|_| String::from("\"\""));
    format!(
        r##"JSON.stringify((function () {{
          var wanted = {needle};
          var best = null;
          var bestArea = Infinity;
          var walker = document.createTreeWalker(document.body, NodeFilter.SHOW_ELEMENT);
          for (var node = walker.nextNode(); node; node = walker.nextNode()) {{
            var text = (node.textContent || "").trim();
            if (text !== wanted) continue;
            var rect = node.getBoundingClientRect();
            if (rect.width <= 0 || rect.height <= 0) continue;
            var style = window.getComputedStyle(node);
            if (style.visibility === "hidden" || style.display === "none") continue;
            if (Number(style.opacity) === 0) continue;
            var area = rect.width * rect.height;
            if (area < bestArea) {{ bestArea = area; best = {{ x: rect.x, y: rect.y, w: rect.width, h: rect.height, tag: node.tagName.toLowerCase() }}; }}
          }}
          return best;
        }})())"##
    )
}
/// Click the control identified by the role and name a page listing reported.
///
/// A page listing answers "what is here" as role plus name, so acting on that
/// answer should take the same pair. Requiring a CSS selector in between forces
/// the caller to translate one vocabulary into another -- by hand, from a name
/// that is a translated string rather than an id -- and the translation is where
/// the mistakes happen. A wrong selector misses; a wrong-but-matching selector
/// hits the wrong thing and still reads as success.
///
/// Names are compared exactly. Substring matching is what let a toolbar and a
/// settings sidebar both answer to one name, and near-misses are the whole
/// problem this verb exists to remove.
pub(crate) fn click_by_name_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let name = request["name"].as_str().unwrap_or("").to_owned();
    if name.trim().is_empty() {
        return Err("click_by_name requires a non-empty name".to_owned());
    }
    let role = request
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let nth = request.get("nth").and_then(Value::as_u64).map(|n| n as usize);
    // Same scoping page_text offers, so a name that exists twice can still be
    // acted on without counting matches by hand.
    let within_name = request
        .get("within")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let (x, y, matched) = with_session(state, request, |session| {
        let tree = session
            .call("Accessibility.getFullAXTree", json!({}))
            .map_err(|error| format!("accessibility tree is unavailable on this target: {error}"))?;
        let mut parent_of: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for node in tree["nodes"].as_array().into_iter().flatten() {
            if let (Some(id), Some(parent)) = (node["nodeId"].as_str(), node["parentId"].as_str()) {
                parent_of.insert(id, parent);
            }
        }
        let chain_of = |id: &str| -> Vec<String> {
            let mut chain = Vec::new();
            let mut cursor = Some(id);
            let mut guard = 0;
            while let Some(current) = cursor {
                chain.push(current.to_owned());
                cursor = parent_of.get(current).copied();
                guard += 1;
                if guard > 64 {
                    break;
                }
            }
            chain
        };
        // The scope is the nearest node carrying that name, so a nested panel
        // wins over an outer one with the same label.
        let scope: Option<Vec<String>> = within_name.as_deref().and_then(|wanted| {
            tree["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|node| {
                    let id = node["nodeId"].as_str()?;
                    let node_name = node["name"]["value"].as_str().unwrap_or("").to_ascii_lowercase();
                    (node_name == wanted).then(|| chain_of(id))
                })
                .min_by_key(Vec::len)
        });
        if within_name.is_some() && scope.is_none() {
            return Err(format!(
                "nothing named {:?} to scope to",
                within_name.unwrap_or_default()
            ));
        }

        let viewport = viewport_metrics(session)?;
        // Centre of the control, after making sure it is actually on screen.
        let reveal = |session: &mut BrowserSession, id: i64| -> Option<(f64, f64, f64, f64)> {
            let Ok((x, y, width, height)) = backend_node_rect(session, id) else {
                return None;
            };
            let clipped = x < 0.0
                || y < 0.0
                || x + width > viewport.width + 1.0
                || y + height > viewport.height + 1.0;
            if clipped {
                return scroll_node_into_view(session, id).ok();
            }
            Some((x, y, width, height))
        };
        let mut found: Vec<(f64, f64, String)> = Vec::new();
        // Names that matched but had nowhere to click. Dropping them in
        // silence is the failure this verb exists to remove: the caller would
        // be told the control is absent while a listing has just named it.
        let mut unplaceable: Vec<String> = Vec::new();
        for node in tree["nodes"].as_array().into_iter().flatten() {
            if node["ignored"].as_bool().unwrap_or(false) {
                continue;
            }
            let node_role = node["role"]["value"].as_str().unwrap_or("").to_owned();
            let node_name = node["name"]["value"].as_str().unwrap_or("").to_owned();
            if let Some(scope) = scope.as_ref() {
                let Some(id) = node["nodeId"].as_str() else {
                    continue;
                };
                if !chain_of(id).iter().any(|ancestor| scope.contains(ancestor)) {
                    continue;
                }
            }
            if node_name != name {
                continue;
            }
            if let Some(wanted) = role.as_deref() {
                if !node_role.to_ascii_lowercase().contains(wanted) {
                    continue;
                }
            }
            if node_role.is_empty() || is_structural_noise(&node_role) {
                continue;
            }
            // A control that is disabled cannot respond. Dispatching a click
            // anyway reports landing and changes nothing, which is the worst
            // answer a tool can give: it looks like the page is broken.
            let disabled = node["properties"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|property| property["name"].as_str() == Some("disabled"))
                .and_then(|property| property["value"]["value"].as_bool())
                .unwrap_or(false);
            if disabled {
                return Err(format!(
                    "{name:?} is disabled: a click would report landing and do nothing."
                ));
            }
            let Some(id) = node["backendDOMNodeId"].as_i64() else {
                unplaceable.push(node_role.clone());
                continue;
            };
            let Some((rx, ry, width, height)) = reveal(session, id) else {
                unplaceable.push(node_role.clone());
                continue;
            };
            if width <= 0.0 || height <= 0.0 {
                unplaceable.push(node_role.clone());
                continue;
            }
            found.push((rx + width / 2.0, ry + height / 2.0, format!("{} {:?}", node_role, node_name)));
        }
        if found.is_empty() {
            // Distinguish "no such control" from "it is not in the scope you
            // named". A dropdown rendered in a portal is a child of the page,
            // not of the dialog it belongs to, so scoping to the dialog hides
            // it -- and a message that only said "not found" sends the caller
            // looking for a control that is plainly on screen.
            if let Some(wanted) = within_name.as_deref() {
                let elsewhere = tree["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|node| node["name"]["value"].as_str() == Some(name.as_str()));
                if elsewhere {
                    return Err(format!(
                        "{name:?} is not inside {wanted:?}; a dropdown is often rendered in a portal.",
                    ));
                }
            }
            if !unplaceable.is_empty() {
                return Err(format!(
                    "{name:?} is listed as {} but has no position to click, so it is not rendered here",
                    unplaceable.join(", "),
                ));
            }
            // The accessibility tree does not contain everything on screen. A
            // provider picker in a portal with its own accessibility context is
            // not in the tree at all: it opens, a screenshot shows every item in
            // it, and the tree returns nothing. Falling back to the rendered
            // document finds what the tree omits, and saying so in the match
            // tells the caller which answer they got.
            let raw = evaluate(session, &dom_text_probe_js(&name))?;
            let text = raw
                .as_str()
                .ok_or_else(|| "dom text probe returned a non-string result".to_owned())?;
            let measured: Value = serde_json::from_str(text)
                .map_err(|error| format!("cannot parse dom probe result: {error}"))?;
            if !measured.is_null() {
                let cx = measured["x"].as_f64().unwrap_or(0.0) + measured["w"].as_f64().unwrap_or(0.0) / 2.0;
                let cy = measured["y"].as_f64().unwrap_or(0.0) + measured["h"].as_f64().unwrap_or(0.0) / 2.0;
                return Ok((
                    cx,
                    cy,
                    format!("{} {:?} (found by text, absent from the accessibility tree)",
                        measured["tag"].as_str().unwrap_or("element"),
                        name),
                ));
            }
            return Err(format!("nothing named {:?} matches", name));
        }
        let pick = match nth {
            Some(n) => found.get(n).ok_or_else(|| {
                format!("{:?} matches {} things, no entry {}", name, found.len(), n)
            })?,
            None if found.len() == 1 => &found[0],
            None => {
                return Err(format!(
                    "{:?} matches {} things: {} -- pass nth to pick one, or within to scope it",
                    name,
                    found.len(),
                    found.iter().map(|entry| entry.2.clone()).collect::<Vec<_>>().join(", ")
                ));
            }
        };
        Ok((pick.0, pick.1, pick.2.clone()))
    })?;
    let hit = with_session(state, request, |session| run_hit_test(session, x, y, None))?;
    with_session(state, request, |session| dispatch_click(session, x, y))?;
    Ok(click_response(
        json!({ "name": name, "matched": matched, "x": x, "y": y }),
        hit,
    ))
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
            | "LineBreak"
            | "Separator"
            | "ScrollBar"
            | "ScrollArea"
            | "RootWebArea"
    )
}

/// Whether a text node is worth listing on its own.
///
/// A button's label is text too, and listing both would put "Save" on the
/// page twice. But a form's validation message is also text, and it is the one
/// thing on screen that says why nothing happened -- dropping it as noise left
/// a page that plainly said "at least one model is required" looking complete.
/// Static text is therefore kept unless it belongs to a control that is listed
/// in its own right, where the control's name already carries the words.
fn is_listable_text(role: &str, name: &str, parent_is_control: bool) -> bool {
    if role != "StaticText" {
        return false;
    }
    !name.trim().is_empty() && !parent_is_control
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
    // Scope the listing to one named container. Two controls can legitimately
    // share a name -- a settings dialog and the chat screen behind it both offer
    // the same one -- and with no way to say which is meant, an agent acts on the
    // wrong element while every response still looks correct.
    let within_name = request
        .get("within")
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
        // nodeId -> parentId, so containment can be answered from the tree
        // itself. Rectangles cannot answer it: a modal's rect is large enough to
        // cover the chat rendered behind it, so a geometric test admits controls
        // the dialog has nothing to do with.
        let mut parent_of: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for node in tree["nodes"].as_array().into_iter().flatten() {
            if let (Some(id), Some(parent)) = (node["nodeId"].as_str(), node["parentId"].as_str()) {
                parent_of.insert(id, parent);
            }
        }
        // One ancestor chain per emitted element, in the same order.
        let mut ancestry: Vec<Vec<String>> = Vec::new();

        let mut elements: Vec<Value> = Vec::new();
        let mut below_fold = 0usize;
        let mut truncated = false;
        for node in tree["nodes"].as_array().into_iter().flatten() {
            if node["ignored"].as_bool().unwrap_or(false) {
                continue;
            }
            let role = node["role"]["value"].as_str().unwrap_or("");
            let name = node["name"]["value"].as_str().unwrap_or("");
            if role.is_empty() {
                continue;
            }
            // Text is kept when it is not already carried by a control, so a
            // validation message survives while a button's own label is not
            // listed twice.
            let inside_control = node["parentId"]
                .as_str()
                .and_then(|parent| {
                    tree["nodes"]
                        .as_array()?
                        .iter()
                        .find(|candidate| candidate["nodeId"].as_str() == Some(parent))
                        .and_then(|candidate| candidate["role"]["value"].as_str())
                })
                .is_some_and(is_clickable_role);
            if is_structural_noise(role) && !is_listable_text(role, name, inside_control) {
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
            // State, not just identity. A disabled button looks exactly like an
            // enabled one in a listing of roles and names, and clicking it
            // reports landing while nothing can happen -- so the reason a page
            // stopped responding has to be readable without a screenshot.
            for flag in ["disabled", "checked", "expanded", "required", "invalid"] {
                if let Some(state) = node["properties"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|property| property["name"].as_str() == Some(flag))
                {
                    if let Some(actual) = state["value"]["value"].as_bool() {
                        entry[flag] = json!(actual);
                    }
                }
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
            if within_name.is_some() {
                let mut chain = Vec::new();
                let mut cursor = node["nodeId"].as_str();
                let mut guard = 0;
                while let Some(id) = cursor {
                    chain.push(id.to_owned());
                    cursor = parent_of.get(id).copied();
                    guard += 1;
                    if guard > 64 {
                        break;
                    }
                }
                ancestry.push(chain);
            }
            elements.push(entry);
        }

        // A control's label is text too, and listing both would put "Save" on
        // the page twice. Deciding that from parent links proved unreliable --
        // the label often sits under a generic wrapper rather than under the
        // button -- so it is decided from the finished list instead: text that
        // merely repeats a listed control's name is that control's label, and
        // text that repeats nothing is a message in its own right.
        let control_names: Vec<String> = elements
            .iter()
            .filter(|entry| {
                entry["role"]
                    .as_str()
                    .is_some_and(is_clickable_role)
            })
            .filter_map(|entry| entry["name"].as_str().map(str::to_ascii_lowercase))
            .collect();
        if !control_names.is_empty() {
            elements.retain(|entry| {
                if entry["role"].as_str() != Some("StaticText") {
                    return true;
                }
                let name = entry["name"].as_str().unwrap_or("").to_ascii_lowercase();
                !control_names.iter().any(|control| *control == name)
            });
        }

        // Keep only what the named container actually contains.
        if let Some(wanted) = within_name.as_deref() {
            let named = elements
                .iter()
                .position(|entry| {
                    entry["name"]
                        .as_str()
                        .is_some_and(|name| name.to_ascii_lowercase() == wanted)
                })
                .ok_or_else(|| format!("no element named {wanted:?} to scope to"))?;
            let named_chain = ancestry
                .get(named)
                .cloned()
                .ok_or_else(|| format!("no element named {wanted:?} to scope to"))?;
            // Naming a dialog by its heading is the natural thing to do, and a
            // heading is a text node rather than a container. The scope is
            // therefore the nearest ancestor of the named node that actually
            // contains things, so a heading scopes to the panel it titles.
            let scope_node = named_chain
                .iter()
                .find(|candidate| {
                    ancestry.iter().any(|chain| {
                        chain.iter().skip(1).any(|ancestor| ancestor == *candidate)
                    })
                })
                .cloned()
                .unwrap_or_else(|| named_chain[0].clone());
            let kept: Vec<Value> = elements
                .iter()
                .zip(ancestry.iter())
                .filter(|(_, chain)| chain.contains(&scope_node))
                .map(|(entry, _)| entry.clone())
                .collect();
            below_fold = kept
                .iter()
                .filter(|entry| entry["inViewport"] == json!(false))
                .count();
            elements = kept;
        }

        Ok(json!({
            "count": elements.len(),
            "truncated": truncated,
            "belowFold": below_fold,
            "elements": elements
        }))
    })
}

fn scroll_probe_js(to: Option<f64>, at: Option<(f64, f64)>) -> String {
    let target = match to {
        Some(value) => value.to_string(),
        None => "null".to_owned(),
    };
    // Kept as two bare numbers rather than a point literal: a nested format!
    // would have its braces consumed by the outer one.
    let (px, py) = match at {
        Some((x, y)) => (x.to_string(), y.to_string()),
        None => ("null".to_owned(), "null".to_owned()),
    };
    format!(
        r##"JSON.stringify((function () {{
          function scrollable(node) {{
            if (!node || node === document.body || node === document.documentElement) return null;
            var style = window.getComputedStyle(node);
            if (!/(auto|scroll|overlay)/.test(style.overflowY)) return null;
            if (node.scrollHeight - node.clientHeight <= 1) return null;
            return node;
          }}
          // A modal usually owns its own scroller, so the document reports
          // nothing below the fold while the panel is full of controls the
          // caller can see named but cannot reach. Prefer the nearest
          // scrollable ancestor of a point inside the region of interest.
          var px = {px};
          var py = {py};
          var el = null;
          if (px !== null && py !== null) {{
            var under = document.elementFromPoint(px, py);
            while (under) {{
              var found = scrollable(under);
              if (found) {{ el = found; break; }}
              under = under.parentElement;
            }}
          }}
          if (!el) el = scrollable(document.scrollingElement || document.documentElement);
          if (!el) el = document.scrollingElement || document.documentElement;
          if (!el) return {{ scrollTop: 0, scrollHeight: 0, viewportHeight: 0, screensBelow: 0, moved: false, scroller: null, scrollerIsDocument: true }};
          var wanted = {target};
          var moved = false;
          if (wanted !== null) {{
            el.scrollTop = wanted;
            moved = true;
          }}
          var isDocument = el === document.scrollingElement || el === document.documentElement;
          var viewportHeight = isDocument ? (window.innerHeight || el.clientHeight || 0) : el.clientHeight;
          var remaining = el.scrollHeight - (el.scrollTop + viewportHeight);
          var label = el.tagName ? el.tagName.toLowerCase() : "document";
          if (el.id) label += "#" + el.id;
          if (typeof el.className === "string" && el.className.trim()) {{
            label += "." + el.className.trim().split(/\s+/).slice(0, 2).join(".");
          }}
          return {{
            scrollTop: el.scrollTop,
            scrollHeight: el.scrollHeight,
            viewportHeight: viewportHeight,
            screensBelow: remaining > 0 && viewportHeight > 0 ? Math.ceil(remaining / viewportHeight) : 0,
            moved: moved,
            scroller: label,
            scrollerIsDocument: isDocument
          }};
        }})())"##
    )
}

/// Report how far the page extends, and optionally scroll to an offset.
///
/// Without this a caller has no way to know a form is three screens tall,
/// which is how a required field gets skipped: the first screen looks
/// complete, and only submitting reveals what was underneath.
pub(crate) fn scroll_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let to = request.get("to").and_then(Value::as_f64);
    let x = request.get("x").and_then(Value::as_f64);
    let y = request.get("y").and_then(Value::as_f64);
    // A point is only meaningful as a pair; half of one would probe the
    // wrong element and report somebody else's scroller.
    let at = match (x, y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    };
    let expression = scroll_probe_js(to, at);
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
        "scroller": measured["scroller"].clone(),
        "scrollerIsDocument": measured["scrollerIsDocument"].clone(),
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
        for role in ["none", "presentation", "generic", "ScrollBar"] {
            assert!(is_structural_noise(role), "{role} should be noise");
        }
        // Substring matching would swallow real content: "GenericButton" and
        // "presentationCard" are not the roles being filtered.
        for role in ["Button", "StaticLabel", "ScrollableRegion", "Generic"] {
            assert!(!is_structural_noise(role), "{role} must survive");
        }
    }

    #[test]
    fn standalone_text_is_listed_but_a_controls_own_label_is_not() {
        // A validation message is the only thing on screen explaining why a
        // button did nothing, so it has to survive as text of its own.
        assert!(is_listable_text("StaticText", "at least one model is required", false));
        // The same words inside a button are already carried by the button's
        // name; listing them again just buries everything else.
        assert!(!is_listable_text("StaticText", "Save", true));
        // Whitespace is not a message.
        assert!(!is_listable_text("StaticText", "   ", false));
        // Only text is judged here; other roles are decided by the noise filter.
        assert!(!is_listable_text("Button", "Save", false));
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
