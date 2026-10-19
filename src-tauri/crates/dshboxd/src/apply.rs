//! Applying a resource document to a container.
//!
//! An apply document declares what the resource layer should be -- `types:` for
//! what a container can see, `copies:` for what state to take -- and applying it
//! makes reality match. That is a better shape for a caller than a sequence of
//! verbs, because the document says the *intent* and the order is derived.
//!
//! This lives in the daemon rather than the CLI because it is not a UI concern:
//! it is a sequence of daemon operations, and anything that has to know the
//! order knows the storage layout too. The CLI and the sandbox agent both call
//! it, so there is one implementation of what a document means and one place to
//! change it. The CLI used to hold the sequence; two callers doing that is how
//! they drift.
//!
//! Applying is idempotent. A type is keyed by kind, container and path, and a
//! copy whose name is already in the store reports `exists` rather than
//! failing or silently making a second one, so re-running a document changes
//! nothing that was already right.
//!
//! @module apply

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{resources, DaemonState};

/// The document. `deny_unknown_fields` because a key that is silently ignored is
/// worse than one that is refused: the caller believes they asked for something
/// that never happened.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    types: Vec<TypeSpec>,
    #[serde(default)]
    copies: Vec<CopySpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeSpec {
    kind: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    container: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopySpec {
    name: String,
    kind: String,
    #[serde(default)]
    from: Option<String>,
    /// An explicit destination, for `kind: path` -- state nothing declared.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    entry: Option<String>,
}

/// What one entry did. The caller gets these per entry, because a document is
/// applied as a whole and a partial failure has to be visible per entry rather
/// than as one opaque error.
struct Outcome {
    action: &'static str,
    target: String,
    status: &'static str,
    detail: Option<String>,
}

impl Outcome {
    fn to_value(&self) -> Value {
        json!({
            "action": self.action,
            "target": self.target,
            "status": self.status,
            "detail": self.detail,
        })
    }
}
/// Apply a document and report what each entry did.
///
/// `document` is the parsed YAML or JSON, or `file` a host path to read it
/// from. Taking the text as well as the path is what lets a caller that never
/// touches the filesystem -- a tool call with the document inline -- use the
/// same path as one that has a file.
pub(crate) fn apply_document_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let dry_run = request
        .get("dryRun")
        .or_else(|| request.get("dry_run"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut source = "the apply document".to_owned();
    let body = match request.get("document").and_then(Value::as_str) {
        Some(text) => text.to_owned(),
        None => {
            let file = request
                .get("file")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .ok_or("apply_document needs a document or a file path")?;
            source = file.to_owned();
            std::fs::read_to_string(file)
                .map_err(|error| format!("cannot read {file}: {error}"))?
        }
    };
    // A UTF-8 BOM is what a Windows editor writes without being asked, and
    // serde_yaml reads it as the start of a second document, so the error comes
    // back saying only that more than one document is not supported -- which
    // says nothing about the byte that caused it. Strip it.
    let body = body.strip_prefix('\u{feff}').unwrap_or(&body);
    let manifest: Manifest = serde_yaml::from_str(body)
        .map_err(|error| format!("{source} is not a valid apply document: {error}"))?;
    if manifest.types.is_empty() && manifest.copies.is_empty() {
        return Err(format!("{source} declares neither types nor copies"));
    }

    let containers = containers_by_name(state)?;
    let mut outcomes: Vec<Outcome> = Vec::new();

    for (index, spec) in manifest.types.iter().enumerate() {
        let container = spec
            .container
            .clone()
            .or_else(|| manifest.container.clone())
            .ok_or_else(|| {
                format!("types[{index}] has no container, and the document names none")
            })?;
        let id = resolve_container(&containers, &container)?;
        let label = spec
            .label
            .clone()
            .unwrap_or_else(|| {
                spec.path.clone().unwrap_or_else(|| spec.kind.clone())
            });
        let target = format!("type {label} ({})", spec.kind);
        if dry_run {
            outcomes.push(Outcome {
                action: "type",
                target,
                status: "would-apply",
                detail: Some(format!("container {container}")),
            });
            continue;
        }
        let mut call = json!({ "kind": spec.kind, "container": id });
        if let Some(path) = &spec.path {
            call["path"] = json!(path);
        }
        if let Some(label) = &spec.label {
            call["label"] = json!(label);
        }
        outcomes.push(match resources::add_resource_view(state, &call) {
            Ok(_) => Outcome {
                action: "type",
                target,
                status: "applied",
                detail: Some(format!("container {container}")),
            },
            Err(error) => Outcome {
                action: "type",
                target,
                status: "failed",
                detail: Some(error),
            },
        });
    }
    let existing = stored_copy_names(state).unwrap_or_default();
    for (index, spec) in manifest.copies.iter().enumerate() {
        let container = spec
            .from
            .clone()
            .or_else(|| spec.container.clone())
            .or_else(|| manifest.container.clone())
            .ok_or_else(|| {
                format!("copies[{index}] has no container, and the document names none")
            })?;
        let id = resolve_container(&containers, &container)?;
        let target = format!("copy {} ({} from {})", spec.name, spec.kind, container);
        if existing.contains(&spec.name) {
            outcomes.push(Outcome {
                action: "copy",
                target,
                status: "exists",
                detail: Some("a copy with this name is already in the store".to_owned()),
            });
            continue;
        }
        if dry_run {
            outcomes.push(Outcome {
                action: "copy",
                target,
                status: "would-apply",
                detail: None,
            });
            continue;
        }
        let mut call = json!({ "id": id, "kind": spec.kind, "name": spec.name });
        if let Some(path) = &spec.path {
            call["dest"] = json!(path);
        }
        if let Some(entry) = &spec.entry {
            call["entry"] = json!(entry);
        }
        outcomes.push(match extract_and_wait(state, &call) {
            Ok(task_id) => Outcome {
                action: "copy",
                target,
                status: "applied",
                detail: Some(task_id),
            },
            Err(error) => Outcome {
                action: "copy",
                target,
                status: "failed",
                detail: Some(error),
            },
        });
    }

    let failed = outcomes
        .iter()
        .filter(|entry| entry.status == "failed")
        .count();
    Ok(json!({
        "dryRun": dry_run,
        "outcomes": outcomes.iter().map(Outcome::to_value).collect::<Vec<_>>(),
        "failed": failed,
    }))
}

/// Enqueue an extract and wait for it, so a copy entry reports what happened.
///
/// The document is applied as a whole, so a caller has to be told whether each
/// copy actually landed. Returning the task id instead hands back a promise with
/// no outcome, which is the thing this whole path exists to avoid.
fn extract_and_wait(state: &DaemonState, request: &Value) -> Result<String, String> {
    let task = match resources::enqueue_resource_extract(state, request)? {
        crate::dispatch::HandlerResult::Async(task) => serde_json::to_value(&task)
            .map_err(|error| format!("cannot serialize the extract task: {error}"))?,
        crate::dispatch::HandlerResult::Sync(value) => value,
    };
    let id = task
        .get("id")
        .and_then(Value::as_str)
        .ok_or("the extract task has no id")?
        .to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        let record = state
            .manager
            .task(&id)
            .map_err(|error| format!("cannot read the extract task: {error}"))?;
        let value = serde_json::to_value(&record)
            .map_err(|error| format!("cannot serialize the extract task: {error}"))?;
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Same rule as the agent side: a failed task may still be rolling back,
        // and only a failed rollback ends it. Stopping at Failed would report a
        // copy as done while the store is still being unwound.
        let rollback = value
            .get("rollbackError")
            .and_then(Value::as_str)
            .map(|error| !error.is_empty())
            .unwrap_or(false);
        let terminal = ["Succeeded", "Cancelled", "Interrupted", "RolledBack"]
            .contains(&status);
        if terminal || (status == "Failed" && rollback) {
            if status == "Succeeded" {
                return Ok(id);
            }
            let detail = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no reason recorded");
            return Err(format!("extract ended {status}: {detail}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("extract is still {status} after ten minutes; task {id}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}
/// Containers as (id, name) pairs, for resolving a document's name to an id.
fn containers_by_name(state: &DaemonState) -> Result<Vec<(String, String)>, String> {
    let value = crate::dispatch::list_containers(state)?;
    let rows = value.as_array().cloned().unwrap_or_default();
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?.to_owned();
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_owned();
            Some((id, name))
        })
        .collect())
}

/// Resolve a document's container reference, which may be an id or a name.
///
/// A document is written by hand, so it names a container the way a person
/// would. Refusing an unknown name with the ones that exist is the difference
/// between a typo being obvious and a document that quietly applies to nothing.
fn resolve_container(containers: &[(String, String)], name: &str) -> Result<String, String> {
    if let Some((id, _)) = containers.iter().find(|(id, _)| id == name) {
        return Ok(id.clone());
    }
    if let Some((id, _)) = containers.iter().find(|(_, known)| known == name) {
        return Ok(id.clone());
    }
    let known = containers
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if known.is_empty() {
        Err(format!("no container named {name:?}, and there are no containers"))
    } else {
        // Naming what does exist is the difference between a typo being obvious
        // and a document that quietly applies to nothing.
        Err(format!("no container named {name:?}; the containers are: {known}"))
    }
}

/// The names already in the copy store, so a second take reports `exists`.
fn stored_copy_names(state: &DaemonState) -> Result<Vec<String>, String> {
    let value = resources::list_resources(state, &json!({}))?;
    let rows = value.as_array().cloned().unwrap_or_default();
    let mut names = Vec::new();
    for row in rows {
        if let Some(name) = row.get("name").and_then(Value::as_str) {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}
