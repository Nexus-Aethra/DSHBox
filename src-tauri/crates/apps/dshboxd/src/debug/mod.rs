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

use base64::Engine as _;
use box_browser::{resolve, validate_browser_path, BrowserCandidate, BrowserSession};
use box_foundation::read_config;
use serde_json::{json, Value};

use crate::dispatch::container_url;
use crate::state::DaemonState;

mod debug_dom;
mod debug_shot;

// This file is the session table and the session lifecycle; debug_dom is the
// CDP operation family and debug_shot is the screenshot path. The globs
// below keep every existing crate::debug::x path working.
pub(crate) use debug_dom::*;
pub(crate) use debug_shot::*;

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

fn lock_sessions(state: &DaemonState) -> Result<std::sync::MutexGuard<'_, Sessions>, String> {
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

/// Close the sessions of containers that are no longer running.
///
/// A debug browser must not outlive the container it was opened to look at.
/// Nothing else ties the two together: the session map is keyed by container
/// id and the browser is a separate process, so stopping a container leaves a
/// headless browser alive on a page whose host is gone. That is a process the
/// user did not ask for, holding a window open, and it accumulates -- one per
/// container that was ever inspected, across restarts.
///
/// So the set of live sessions is reconciled against the containers that are
/// actually running before any new one is opened. Dropping the entry also drops
/// the `BrowserSession`, which kills the process.
///
/// A container whose liveness cannot be read at all is left alone: an
/// unreadable record is not evidence that the container is gone, and closing a
/// browser over a failed lookup would be the worse mistake.
fn close_sessions_of_stopped_containers(sessions: &mut Sessions) {
    let stopped: Vec<String> = sessions
        .keys()
        .filter(|id| container_is_stopped(id))
        .cloned()
        .collect();
    for id in stopped {
        sessions.remove(&id);
    }
}

/// True only when the daemon can positively say this container is not running.
fn container_is_stopped(id: &str) -> bool {
    match crate::dispatch::container_url_probe(id) {
        Ok(_) => false,
        // A running container answers with a URL. Anything else -- no record,
        // a dead pid, a host that is not yet up -- means the container is not
        // something we can attach to, and the browser has nothing left to show.
        Err(_) => true,
    }
}
/// Launch (or replace) the headless session attached to a container.
pub(crate) fn open_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // Keyed by the canonical id: an alias would store a session under a name
    // the liveness sweep cannot probe, and the same container opened twice by
    // two spellings would hold two headless browsers.
    let id = session_id(state, request)?;
    let browser = current_browser()?;
    let url = container_url(state, &id)?;
    let profile = profile_dir(state, &id)?;

    let mut sessions = lock_sessions(state)?;
    // Sweep first: inspecting any container is a good moment to notice that a
    // different one stopped and is still holding a browser open.
    close_sessions_of_stopped_containers(&mut sessions);
    // Re-opening replaces: a stale session is attached to a port that may
    // have been recycled by an unrelated process, which is worse than a
    // fresh launch.
    sessions.remove(&id);
    // A caller may ask for a size on open; otherwise the browser's default
    // 800x600 applies, which folds pages that are not folded on a real screen.
    let viewport = (
        request["width"]
            .as_u64()
            .filter(|value| *value > 0)
            .unwrap_or(BrowserSession::DEFAULT_VIEWPORT.0 as u64),
        request["height"]
            .as_u64()
            .filter(|value| *value > 0)
            .unwrap_or(BrowserSession::DEFAULT_VIEWPORT.1 as u64),
    );
    let session = BrowserSession::launch(
        &browser.path,
        &url,
        profile,
        (viewport.0 as u32, viewport.1 as u32),
    )?;
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
    let id = session_id(state, request)?;
    let mut sessions = lock_sessions(state)?;
    let existed = sessions.remove(&id).is_some();
    Ok(json!({ "id": id, "closed": existed }))
}

/**
 * Console output and page exceptions since the last read, oldest first.
 *
 * Two DevTools events carry this and they do not look alike:
 *
 *   Runtime.consoleAPICalled  { type, args, timestamp, stackTrace }
 *   Runtime.exceptionThrown  { exceptionDetails: { text, exception, stackTrace } }
 *
 * Both are flattened to { level, text, source, count } because an agent
 * deciding what to do next wants "what did the page say", not which of the two
 * shapes it arrived in.
 */
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
    let resolved = session.call(
        "DOM.resolveNode",
        json!({ "backendNodeId": backend_node_id }),
    )?;
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
/// Find one control by role and name, and return the point that is its centre.
fn locate_by_name(
    session: &mut BrowserSession,
    search: NameSearch,
) -> Result<(f64, f64, String), String> {
    let NameSearch {
        name,
        role,
        nth,
        within_name,
    } = search;
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
                let node_name = node["name"]["value"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase();
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
        found.push((
            rx + width / 2.0,
            ry + height / 2.0,
            format!("{} {:?}", node_role, node_name),
        ));
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
            let cx =
                measured["x"].as_f64().unwrap_or(0.0) + measured["w"].as_f64().unwrap_or(0.0) / 2.0;
            let cy =
                measured["y"].as_f64().unwrap_or(0.0) + measured["h"].as_f64().unwrap_or(0.0) / 2.0;
            return Ok((
                cx,
                cy,
                format!(
                    "{} {:?} (found by text, absent from the accessibility tree)",
                    measured["tag"].as_str().unwrap_or("element"),
                    name
                ),
            ));
        }
        // Offer the near misses with the failure. A caller who typed a
        // name that does not exist should not have to go and re-read the
        // page to discover what the page called it.
        let near = near_miss_names(&tree, &name, 8);
        if !near.is_empty() {
            return Err(format!(
                "nothing named {:?} matches. Closest names: {}",
                name,
                near.join(", "),
            ));
        }
        return Err(format!("nothing named {:?} matches", name));
    }
    let pick = match nth {
        Some(n) => found
            .get(n)
            .ok_or_else(|| format!("{:?} matches {} things, no entry {}", name, found.len(), n))?,
        None if found.len() == 1 => &found[0],
        None => {
            return Err(format!(
                "{:?} matches {} things: {} -- pass nth to pick one, or within to scope it",
                name,
                found.len(),
                found
                    .iter()
                    .map(|entry| entry.2.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    };
    Ok((pick.0, pick.1, pick.2.clone()))
}

/// Press and release one named key.
///
/// Separate from `type_text` because a submit keystroke carries no text of
/// its own: an Enter that submits a form has to arrive as a real key event
/// for the form to hear about it.
pub(crate) fn press_key_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let name = request["key"].as_str().unwrap_or("").trim().to_owned();
    let spec =
        key_spec(&name).ok_or_else(|| format!("press_key does not know the key {name:?}"))?;
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
            .filter(|entry| entry["role"].as_str().is_some_and(is_clickable_role))
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
                    ancestry
                        .iter()
                        .any(|chain| chain.iter().skip(1).any(|ancestor| ancestor == *candidate))
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

        // The page's own location travels with the listing. The container's host
        // rebinds its port on every start, so "what is on screen" and "is this
        // screen even still connected" are the same question, and a listing that
        // answers one without the other sends the caller to a screenshot to find
        // out that it is looking at a dead port.
        let (url, title) = page_location(session)?;
        Ok(json!({
            "count": elements.len(),
            "truncated": truncated,
            "belowFold": below_fold,
            "url": url,
            "title": title,
            "elements": elements
        }))
    })
}