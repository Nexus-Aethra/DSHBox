//! CDP operations: query the DOM, click, type, scroll, navigate, hover, read
//! page text, and resize the viewport.
//!
//! The largest of the three, and honestly so: the DevTools Protocol has this
//! many operations, and splitting them further would separate
//! dispatch_click from click_element_rpc -- two halves of one call -- which
//! makes finding either one harder, not easier.

use super::*;

pub(crate) fn console_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // Chrome's own level names, plus the two the console API adds that are not
    // levels at all but arrive on the same channel.
    let wanted: Option<Vec<String>> = request.get("levels").and_then(Value::as_array).map(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .map(|level| level.to_owned())
            .collect()
    });
    let limit = request
        .get("limit")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(50)
        .min(500) as usize;

    let events = with_session(state, request, |session| session.drain_events())?;
    let mut entries: Vec<Value> = Vec::new();
    for event in &events {
        let method = event["method"].as_str().unwrap_or("");
        let params = &event["params"];
        let (level, text, source) = match method {
            "Runtime.consoleAPICalled" => {
                let level = params["type"].as_str().unwrap_or("log").to_owned();
                let mut parts: Vec<String> = Vec::new();
                for arg in params["args"].as_array().cloned().unwrap_or_default() {
                    parts.push(render_remote_object(&arg));
                }
                (level, parts.join(" "), first_frame(&params["stackTrace"]))
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let text = details["exception"]["description"]
                    .as_str()
                    .or_else(|| details["exception"]["value"].as_str())
                    .or_else(|| details["text"].as_str())
                    .unwrap_or("uncaught error")
                    .to_owned();
                (
                    "error".to_owned(),
                    text,
                    first_frame(&details["stackTrace"]),
                )
            }
            // Log.entryAdded and the network family are real, but they are
            // volume, not signal: a page load emits dozens and an agent reading
            // them learns nothing it cannot get from box_page_text.
            _ => continue,
        };
        match &wanted {
            Some(filter) if !filter.iter().any(|want| *want == level) => continue,
            _ => {}
        }
        entries.push(json!({
            "level": level,
            "text": text,
            "source": source,
        }));
    }

    // A page that logs the same failure in a loop would otherwise hand back
    // fifty identical lines and hide the three other things that also happened.
    let mut counts: Vec<(String, String, Option<String>, u64)> = Vec::new();
    for entry in entries {
        let level = entry["level"].as_str().unwrap_or("").to_owned();
        let text = entry["text"].as_str().unwrap_or("").to_owned();
        let source = entry["source"].as_str().map(str::to_owned);
        match counts
            .iter_mut()
            .find(|(l, t, s, _)| *l == level && *t == text && *s == source)
        {
            Some((_, _, _, count)) => *count += 1,
            None => counts.push((level, text, source, 1)),
        }
    }
    let total = counts.len();
    let mut out: Vec<Value> = counts
        .into_iter()
        .map(|(level, text, source, count)| {
            json!({
                "level": level,
                "text": text,
                "source": source,
                "count": count,
            })
        })
        .collect();
    if out.len() > limit {
        out.truncate(limit);
    }
    // Newest last is how a page logs, and how an agent reads; a tail is the
    // part that explains what just broke.
    Ok(json!({
        "entries": out,
        "total": total,
        "truncated": total > limit,
        // How many DevTools events arrived at all, before level filtering and
        // before the two families that are dropped as noise. A caller that sees
        // 0 raw and 0 entries knows the page was quiet; one that sees many raw
        // and no entries knows the filter or the event shapes are wrong, and
        // those are very different bugs to chase.
        "raw": events.len(),
        "methods": events.iter().filter_map(|e| e["method"].as_str()).collect::<Vec<_>>(),
    }))
}


/// One argument of a console call, as text.
///
/// CDP carries the value in whichever shape it happens to have: a string is a
/// string, an error is an Error object, and everything else only exists as a
/// preview. Falling back to the type name is better than dropping the argument,
/// because "undefined" where a value was expected is itself the finding.
pub(super) fn render_remote_object(arg: &Value) -> String {
    if let Some(text) = arg["value"].as_str() {
        return text.to_owned();
    }
    if let Some(description) = arg["description"].as_str() {
        return description.to_owned();
    }
    if let Some(text) = arg["value"].as_i64() {
        return text.to_string();
    }
    if let Some(text) = arg["value"].as_f64() {
        return text.to_string();
    }
    arg["type"].as_str().unwrap_or("").to_owned()
}

/// "file.js:12:5" for the first frame, when the page gave one.
pub(super) fn first_frame(trace: &Value) -> Option<String> {
    let frame = &trace["callFrames"][0];
    let url = frame["url"].as_str().unwrap_or("");
    let line = frame["lineNumber"].as_i64().unwrap_or(-1);
    if url.is_empty() || line < 0 {
        return None;
    }
    let file = url.rsplit('/').next().unwrap_or(url);
    Some(format!("{file}:{}", line + 1))
}

/// Where the attached page actually is, as the page itself reports it.
///
/// Read from the document rather than from the URL the session was launched
/// with, because the two come apart: the container's host rebinds its loopback
/// port on every start, a login redirects, and a hash route is not in the URL
/// we asked for. The caller is asking "what is this browser showing", so the
/// answer has to be the browser's.
pub(super) fn page_location(session: &mut BrowserSession) -> Result<(String, Option<String>), String> {
    let url = session.current_url()?;
    let title = session.current_title().ok().filter(|text| !text.is_empty());
    Ok((url, title))
}

/// The reference a request carries, resolved to the id its session is keyed by.
///
/// Every page tool arrives here, so this is the one place a name or a bare
/// timestamp can be accepted without each verb learning the rules: sessions are
/// keyed by the canonical id (see `open_rpc`), and a lookup by alias would
/// otherwise report "no debug session" for a container that is being looked at.
pub(super) fn session_id(state: &DaemonState, request: &Value) -> Result<String, String> {
    let raw = request["id"].as_str().unwrap_or("");
    if raw.is_empty() {
        return Err("a container id is required".to_owned());
    }
    crate::resolve::resolve_container_in_root(&crate::resolve::runtime_root(state)?, raw)
        .map(|found| found.id)
}

/// Run one CDP call against a container's session.
pub(super) fn with_session<T>(
    state: &DaemonState,
    request: &Value,
    action: impl FnOnce(&mut BrowserSession) -> Result<T, String>,
) -> Result<T, String> {
    let id = session_id(state, request)?;
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
pub(super) fn is_dead_session_error(error: &str) -> bool {
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
pub(super) fn evaluate(session: &mut BrowserSession, expression: &str) -> Result<Value, String> {
    let result = session.call(
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
            // Off by default. With it on, a page that throws inside the
            // expression is reported as Runtime.exceptionThrown as well as in
            // the reply, so debug_console sees it -- which is the whole point
            // of the console: a caller that ran something into a page error
            // should be able to find out from one place.
            "reportExceptions": true,
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


/// Build the JS that describes every element matching a selector.
///
/// One multi-line literal rather than a chain of adjacent pieces: Rust does
/// not concatenate neighbouring string literals the way C does, so the
/// split-and-join form does not compile.
pub(super) fn element_probe_js(selector: &str, limit: usize) -> String {
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

pub(super) fn parse_elements(value: Value) -> Result<Vec<Value>, String> {
    let text = value
        .as_str()
        .ok_or_else(|| "element query returned a non-string result".to_owned())?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(text).map_err(|error| format!("cannot parse element list: {error}"))
}

/// List the page elements matching a CSS selector.
pub(crate) fn query_elements_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
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


pub(super) fn dispatch_click(session: &mut BrowserSession, x: f64, y: f64) -> Result<(), String> {
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
    let hit = with_session(state, request, |session| {
        run_hit_test(session, x, y, Some(&selector))
    })?;
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
pub(super) fn dom_text_probe_js(name: &str) -> String {
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
/// Names close enough to what was asked for to be worth offering.
///
/// Forms name their fields after what they are, and the index is part of the
/// name: a catalogue row is "Model ID 1", not "Model ID". An exact match
/// fails on that, and a bare "nothing matches" leaves the caller to re-read the
/// whole page guessing. So the near misses come back with the answer -- close
/// first, then anything sharing a long prefix, capped so the reply stays short.
pub(super) fn near_miss_names(tree: &Value, wanted: &str, limit: usize) -> Vec<String> {
    let wanted = wanted.to_ascii_lowercase();
    let mut close: Vec<(usize, String)> = Vec::new();
    for node in tree["nodes"].as_array().into_iter().flatten() {
        if node["ignored"].as_bool().unwrap_or(false) {
            continue;
        }
        let name = node["name"]["value"].as_str().unwrap_or("").to_owned();
        if name.is_empty() {
            continue;
        }
        let lowered = name.to_ascii_lowercase();
        // Rank by how much of the wanted name survives, so a field called
        // "Model ID 1" outranks an unrelated "Model" three entries away.
        let score = if lowered == wanted {
            0
        } else if lowered.starts_with(&wanted) || wanted.starts_with(&lowered) {
            1 + wanted.chars().count().abs_diff(lowered.chars().count())
        } else if lowered.contains(&wanted) || wanted.contains(&lowered) {
            50 + wanted.chars().count().abs_diff(lowered.chars().count())
        } else {
            let shared = lowered
                .chars()
                .zip(wanted.chars())
                .take_while(|(a, b)| a == b)
                .count();
            if shared >= 3 && shared * 2 >= wanted.chars().count() {
                100 - shared
            } else {
                continue;
            }
        };
        let role = node["role"]["value"].as_str().unwrap_or("");
        close.push((score, format!("{role} {name:?}")));
    }
    close.sort_by(|a, b| a.0.cmp(&b.0));
    close.dedup_by(|a, b| a.1 == b.1);
    close
        .into_iter()
        .take(limit)
        .map(|(_, entry)| entry)
        .collect()
}

/// Names are compared exactly. Substring matching is what let a toolbar and a
/// settings sidebar both answer to one name, and near-misses are the whole
/// problem a naming verb exists to remove.
///
/// What a naming verb is looking for, and how its caller disambiguates a tie.
///
/// Held as one value because two verbs ask the same question: click_by_name
/// wants the coordinates to press, and hover wants the same coordinates to
/// park a pointer on. Reading them separately is how the two would drift into
/// answering "which control?" differently, and a caller that hovers one thing
/// and clicks another would never see it coming.
#[derive(Clone)]
pub(super) struct NameSearch {
    pub(super) name: String,
    pub(super) role: Option<String>,
    pub(super) nth: Option<usize>,
    pub(super) within_name: Option<String>,
}

/// Parse the naming arguments out of a request.
pub(super) fn name_search_from(request: &Value, verb: &str) -> Result<NameSearch, String> {
    let name = request["name"].as_str().unwrap_or("").to_owned();
    if name.trim().is_empty() {
        return Err(format!("{verb} requires a non-empty name"));
    }
    let role = request
        .get("role")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let nth = request
        .get("nth")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    // Same scoping page_text offers, so a name that exists twice can still be
    // acted on without counting matches by hand.
    let within_name = request
        .get("within")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    Ok(NameSearch {
        name,
        role,
        nth,
        within_name,
    })
}


/// Click the control named by a page listing.
///
/// The locating is `locate_by_name`, shared with hover: an agent reaches a
/// hover-gated control by naming it once and then doing two different things at
/// the same point, and if the two verbs resolved the name differently the agent
/// would hover one control and click another with no sign of it.
pub(crate) fn click_by_name_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let search = name_search_from(request, "click_by_name")?;
    let name = search.name.clone();
    let (x, y, matched) = with_session(state, request, |session| locate_by_name(session, search))?;
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
pub(super) struct KeySpec {
    pub(super) key: &'static str,
    pub(super) code: &'static str,
    pub(super) key_code: u32,
    pub(super) text: Option<&'static str>,
}

pub(super) fn key_spec(name: &str) -> Option<KeySpec> {
    let spec = match name {
        "Enter" | "submit" | "return" => KeySpec {
            key: "Enter",
            code: "Enter",
            key_code: 13,
            text: Some("\r"),
        },
        "Tab" => KeySpec {
            key: "Tab",
            code: "Tab",
            key_code: 9,
            text: Some("\t"),
        },
        "Escape" | "esc" => KeySpec {
            key: "Escape",
            code: "Escape",
            key_code: 27,
            text: None,
        },
        "Backspace" => KeySpec {
            key: "Backspace",
            code: "Backspace",
            key_code: 8,
            text: None,
        },
        "Delete" | "del" => KeySpec {
            key: "Delete",
            code: "Delete",
            key_code: 46,
            text: None,
        },
        "ArrowUp" | "up" => KeySpec {
            key: "ArrowUp",
            code: "ArrowUp",
            key_code: 38,
            text: None,
        },
        "ArrowDown" | "down" => KeySpec {
            key: "ArrowDown",
            code: "ArrowDown",
            key_code: 40,
            text: None,
        },
        "ArrowLeft" => KeySpec {
            key: "ArrowLeft",
            code: "ArrowLeft",
            key_code: 37,
            text: None,
        },
        "ArrowRight" => KeySpec {
            key: "ArrowRight",
            code: "ArrowRight",
            key_code: 39,
            text: None,
        },
        "Home" => KeySpec {
            key: "Home",
            code: "Home",
            key_code: 36,
            text: None,
        },
        "End" => KeySpec {
            key: "End",
            code: "End",
            key_code: 35,
            text: None,
        },
        "PageUp" => KeySpec {
            key: "PageUp",
            code: "PageUp",
            key_code: 33,
            text: None,
        },
        "PageDown" => KeySpec {
            key: "PageDown",
            code: "PageDown",
            key_code: 34,
            text: None,
        },
        "Shift" => KeySpec {
            key: "Shift",
            code: "ShiftLeft",
            key_code: 16,
            text: None,
        },
        "Control" | "ctrl" => KeySpec {
            key: "Control",
            code: "ControlLeft",
            key_code: 17,
            text: None,
        },
        "Alt" => KeySpec {
            key: "Alt",
            code: "AltLeft",
            key_code: 18,
            text: None,
        },
        _ => return None,
    };
    Some(spec)
}


/// Default entries returned by `debug_page_text`.
pub(super) const DEFAULT_PAGE_TEXT: usize = 200;
/// Hard cap on entries returned by `debug_page_text`.
///
/// Higher than MAX_ELEMENTS on purpose: the accessibility tree is already
/// filtered to things a person would act on, so the useful entries are a
/// small fraction of a page's DOM and a 200 ceiling would cut them off before
/// the caller saw the fold.
pub(super) const MAX_PAGE_TEXT: usize = 1000;

/// Accessibility roles that are structure rather than content.
///
/// A chat UI's tree is mostly these. Keeping them would bury every actionable
/// line, and the caller's job is to find the actionable lines.
pub(super) fn is_structural_noise(role: &str) -> bool {
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
pub(super) fn is_listable_text(role: &str, name: &str, parent_is_control: bool) -> bool {
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
pub(super) fn is_clickable_role(role: &str) -> bool {
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
pub(super) struct Viewport {
    pub(super) width: f64,
    pub(super) height: f64,
}

pub(super) fn viewport_metrics(session: &mut BrowserSession) -> Result<Viewport, String> {
    let metrics = session.call("Page.getLayoutMetrics", json!({}))?;
    let visual = &metrics["cssVisualViewport"];
    let layout = &metrics["cssLayoutViewport"];
    let number = |value: &Value, fallback: &Value| -> f64 {
        value.as_f64().or_else(|| fallback.as_f64()).unwrap_or(0.0)
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
pub(super) fn backend_node_rect(
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
pub(super) fn quad_bounds(coordinates: &[f64]) -> Option<(f64, f64, f64, f64)> {
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

pub(super) fn scroll_probe_js(to: Option<f64>, at: Option<(f64, f64)>) -> String {
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
pub(super) struct HitTest {
    pub(super) landed: bool,
    pub(super) hit_tag: Option<String>,
    pub(super) hit_text: Option<String>,
    pub(super) occluded_by: Option<String>,
}

/// JS that reports what `document.elementFromPoint` finds at a point.
///
/// With a selector, the hit is also compared against the intended node. A
/// click counts as landed when the hit *is* the target, sits inside it, or
/// contains it: a button's inner span and the button itself are the same
/// click, while an overlay on top of it is a different one.
pub(super) fn hit_test_js(x: f64, y: f64, selector: Option<&str>) -> String {
    let target_check = match selector {
        Some(_) => "var target = document.querySelector(SELECTOR);".to_owned(),
        None => "var target = null;".to_owned(),
    };
    let comparison = match selector {
        Some(_) => r#"out.landed = !!hit && !!target
                 && (target === hit || target.contains(hit) || hit.contains(target));"#
            .to_owned(),
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

pub(super) fn run_hit_test(
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
pub(super) fn click_response(base: Value, hit: HitTest) -> Value {
    let mut response = base;
    response["landed"] = json!(hit.landed);
    response["hitTag"] = json!(hit.hit_tag);
    response["hitText"] = json!(hit.hit_text);
    if !hit.landed {
        response["occludedBy"] = json!(hit.occluded_by);
    }
    response
}


/// Move the attached page to another URL.
///
/// The container's host binds a fresh loopback port every start
/// (`allocate_loopback_port` binds port 0 and lets the OS choose), so a page
/// opened against a running container is pointed at a port that dies the
/// moment the container restarts. Nothing else can put it back: the session
/// keeps its browser, the tools keep working, and the caller sees a page that
/// will never answer again. This is the recovery, and it is also the general
/// "go somewhere else" verb a debugging surface needs.
///
/// With no URL the container's own live URL is used, because that is what the
/// caller means 99% of the time: "take me back to the app", not "navigate
/// somewhere I did not name".
pub(crate) fn navigate_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let wanted = request["url"].as_str().map(str::trim).filter(|text| !text.is_empty());
    let id = session_id(state, request)?;
    let target = match wanted {
        Some(url) => url.to_owned(),
        // The authenticated form, not the bare host URL: DSH 0.1.2+ answers a
        // tokenless request with 401, so a navigation to the bare URL lands a
        // headless browser on an "authentication required" page that looks
        // exactly like a broken container.
        None => container_url(state, &id)?,
    };
    // The debug browser drives the container's own loopback origins, and
    // everything the page shows is readable back through box_evaluate and
    // screenshots — so a file:// URL would hand local files to the agent.
    // Same rule the DSH webview follows; loopback http(s) only.
    let loopback = target.starts_with("http://127.0.0.1")
        || target.starts_with("https://127.0.0.1")
        || target.starts_with("http://localhost")
        || target.starts_with("https://localhost")
        || target.starts_with("http://[::1]")
        || target.starts_with("https://[::1]");
    if !loopback {
        return Err(format!(
            "navigate is restricted to the container's loopback origin (http(s) on 127.0.0.1, localhost or [::1]); refusing {target:?}"
        ));
    }
    with_session(state, request, |session| session.navigate(&target))?;
    // Settled from the page rather than echoed from the request: a navigation
    // is not where the caller ends up (login redirects, SPA routing), and a
    // reply that reports the asked-for URL teaches the caller to distrust it.
    let (url, title) = with_session(state, request, page_location)?;
    Ok(json!({ "id": id, "url": url, "title": title, "navigatedTo": target }))
}


/// Park the pointer on one point of the page.
///
/// A click already moves first (see `dispatch_click`), so this is not about
/// reaching a click-only control: it is about the frame *between* the move and
/// whatever comes next. Row actions in DSH's own UI are `display: none` until
/// `:hover`, which means they are absent from the accessibility tree a listing
/// reads and from the screenshot a caller takes, and there was no way to make
/// them appear and then look. Hovering, then inspecting, is that missing frame.
///
/// Two leaves move: the target and then the point one element in from it, so
/// CSS `:hover` and any `pointerenter` listener see a pointer that settled
/// rather than one that crossed the boundary on the way past.
pub(super) fn dispatch_hover(session: &mut BrowserSession, x: f64, y: f64) -> Result<(), String> {
    for (dx, dy) in [(0.0, 0.0), (0.5, 0.5)] {
        session.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseMoved",
                "x": x + dx,
                "y": y + dy,
                "button": "none",
                "buttons": 0,
            }),
        )?;
    }
    Ok(())
}

/// Hover one point, or one control named the way a page listing names it.
pub(crate) fn hover_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let has_point = request["x"].as_f64().is_some() && request["y"].as_f64().is_some();
    let has_name = !request["name"].as_str().unwrap_or("").trim().is_empty();
    if !has_point && !has_name {
        return Err("hover requires x and y, or a name".to_owned());
    }
    let (x, y, matched) = if has_name {
        let search = name_search_from(request, "hover")?;
        let (x, y, matched) = with_session(state, request, |session| locate_by_name(session, search))?;
        (x, y, json!(matched))
    } else {
        let x = request["x"].as_f64().unwrap_or(0.0);
        let y = request["y"].as_f64().unwrap_or(0.0);
        if !x.is_finite() || !y.is_finite() {
            return Err("hover coordinates must be finite numbers".to_owned());
        }
        (x, y, Value::Null)
    };
    with_session(state, request, |session| dispatch_hover(session, x, y))?;
    // What the hover revealed, read back. The point of hovering is that the
    // page changed, and a reply that says only "hovered" makes the caller
    // take a second call -- a screenshot or a listing -- to find out whether
    // it worked. The hit test is the cheap version of that answer.
    let hit = with_session(state, request, |session| run_hit_test(session, x, y, None))?;
    let (url, _) = with_session(state, request, page_location)?;
    // Every key present, null where it does not apply — the same shape the
    // click verbs answer with, so one schema describes both and a caller
    // never has to distinguish an absent field from an inapplicable one.
    Ok(json!({
        "url": url,
        "x": x,
        "y": y,
        "landed": hit.landed,
        "hitTag": hit.hit_tag,
        "hitText": hit.hit_text,
        "matched": matched,
        "occludedBy": hit.occluded_by,
    }))
}


/// Run an expression in the page and hand back its value.
///
/// Already used by every other verb here -- `evaluate` is what a hit test, an
/// element query and a page title read go through -- so this exposes the
/// mechanism rather than adding one. The gap it closes: a caller that needs a
/// fact the typed verbs do not answer for (an attribute, a computed style,
/// whether an element exists) had no way to ask, and had to guess from a
/// screenshot.
///
/// The page is the whole scope. `box_evaluate` runs inside the debugged page
/// the same way the page's own scripts do, which is no wider than what a page
/// can already do to itself, and nothing narrower would be able to read a
/// React-owned value.
pub(crate) fn evaluate_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let expression = request["expression"].as_str().unwrap_or("").to_owned();
    if expression.trim().is_empty() {
        return Err("evaluate requires an expression".to_owned());
    }
    // Serialized as text rather than returned as JSON: an object with a
    // getter, a DOM node, or a cycle cannot be put on the wire, and a page's
    // own value is what the caller wanted to see.
    let wrapped = format!(
        // String(...) around the stringify: an expression evaluating to
        // undefined makes JSON.stringify return undefined, the CDP reply
        // then carries no value at all, and the schema the tool declares
        // (result: string, required) is violated by an ordinary statement.
        "(function () {{ var v = (eval({})); return (typeof v === 'string') ? v : String(JSON.stringify(v, null, 0)); }})()",
        Value::String(expression.clone()).to_string()
    );
    let value = with_session(state, request, |session| evaluate(session, &wrapped))?;
    let (url, _) = with_session(state, request, page_location)?;
    Ok(json!({ "url": url, "result": value }))
}

/// JS that names the focused editable field, or null when there is none.
///
/// "Editable" is the page's own answer rather than a list of tags: a disabled
/// or read-only input reports false for isContentEditable and takes focus in
/// some frameworks, so it is excluded explicitly.
pub(super) const FOCUS_PROBE_JS: &str = r#"(function () {
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
pub(super) fn focused_field_tag(session: &mut BrowserSession) -> Result<Option<String>, String> {
    let value = evaluate(session, FOCUS_PROBE_JS)?;
    Ok(value
        .as_str()
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned))
}

#[cfg(test)]
#[path = "debug_tests.rs"]
mod tests;

/// `debug_set_viewport` — resize a session's page, and report the size now in effect.
///
/// The window a session opens at is a guess. A page that fits in one is fine,
/// and a page that needs a tall scroll is a stream of partial views: the listing
/// marks controls below the fold that the caller would rather see in one pass,
/// and a click near the edge lands somewhere the agent was not looking at.
/// Resizing is the cheap correction, and it must not cost a relaunch -- losing
/// the session to change its geometry would make the correction worse than the
/// complaint.
///
/// Reported back, rather than assumed: whether the override actually took is
/// the emulator's answer, not this function's, and a caller that believes a
/// resize it did not get will misread every coordinate after it.
pub(crate) fn set_viewport_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        return Err("debug_set_viewport requires a container id".to_owned());
    }
    let width = request["width"].as_u64().filter(|value| *value > 0);
    let height = request["height"].as_u64().filter(|value| *value > 0);
    let (width, height) = match (width, height) {
        (Some(width), Some(height)) => (width, height),
        _ => return Err("debug_set_viewport requires a positive width and height".to_owned()),
    };
    // Chrome refuses absurd sizes by silently clamping, so a caller asking for
    // the whole world back gets a number it did not ask for. Bounded to
    // something a real display can do, and said in the error rather than
    // returned as a surprise.
    if !(320..=7680).contains(&width) || !(240..=4320).contains(&height) {
        return Err(format!(
            "{width}x{height} is not a usable viewport; use 320..7680 by 240..4320"
        ));
    }
    with_session(state, request, |session| {
        session.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": 1,
                "mobile": false,
            }),
        )?;
        let metrics = session
            .call("Page.getLayoutMetrics", json!({}))
            .map_err(|error| format!("viewport was set but its size could not be read: {error}"))?;
        let actual_width = metrics["cssLayoutViewport"]["clientWidth"]
            .as_u64()
            .or_else(|| metrics["layoutViewport"]["clientWidth"].as_u64())
            .unwrap_or(width);
        let actual_height = metrics["cssLayoutViewport"]["clientHeight"]
            .as_u64()
            .or_else(|| metrics["layoutViewport"]["clientHeight"].as_u64())
            .unwrap_or(height);
        Ok(json!({
            "id": id,
            "requested": { "width": width, "height": height },
            "width": actual_width,
            "height": actual_height,
            "clamped": actual_width != width || actual_height != height,
        }))
    })
}
