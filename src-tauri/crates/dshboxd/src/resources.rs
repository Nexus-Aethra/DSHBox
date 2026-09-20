//! Container resource RPCs: what a container persists, and moving it in or out.
//!
//! Extraction and injection are background tasks like every other long
//! operation, so a multi-gigabyte session history reports progress and can be
//! cancelled instead of blocking the connection thread.

use box_foundation::collection::Collection;
use box_foundation::now_seconds;
use box_resources::kinds::Conflict;
use box_resources::record::{self, ResourceRecord};
use box_resources::{discover, transfer, ResolvedKind, Shape};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::dispatch::{enqueue_task_worker, HandlerResult};
use crate::lifecycle::{start_dsh_container_inner, stop_dsh_container};
use crate::state::DaemonState;

/// The runtime root, or an error when Box storage was never configured.
fn runtime_root(state: &DaemonState) -> Result<PathBuf, String> {
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    paths
        .runtime
        .clone()
        .ok_or_else(|| "DSH Box storage is not configured".to_owned())
}

fn container_dir(root: &Path, id: &str) -> Result<PathBuf, String> {
    let directory = root.join("instances").join(id);
    if directory.is_dir() {
        Ok(directory)
    } else {
        Err(format!("container not found: {id}"))
    }
}

/// A container records its profile in `container.json`; `web` is what the
/// daemon falls back to when the file predates that field.
/// Read one file out of a container as a YAML tree, so the UI can show what is
/// in it and let the user edit one block by its key path. A file that is not
/// YAML still answers, with a single node: the user may be looking at a JSON
/// file, and "this is not a YAML document" is the honest answer.
pub(crate) fn read_resource_tree(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?;
    let rel = request["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .ok_or("expected a container-relative path")?;
    let section: Vec<String> = request["section"]
        .as_array()
        .map(|keys| keys.iter().filter_map(|key| key.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let root = runtime_root(state)?;
    let container = container_dir(&root, id)?;
    let file = transfer::safe_join(&container, rel)?;
    let text = std::fs::read_to_string(&file)
        .map_err(|error| format!("cannot read {rel}: {error}"))?;
    let document: serde_yaml::Value = serde_yaml::from_str(&text)
        .map_err(|error| format!("{rel} is not a YAML document: {error}"))?;
    let mut nodes = Vec::new();
    collect_yaml_nodes(&document, &mut Vec::new(), 0, &mut nodes);
    // The editor starts at the section the caller asked for, or the whole file.
    let selected = if section.is_empty() {
        serde_yaml::to_string(&document).map_err(|error| error.to_string())?
    } else {
        match yaml_at(&document, &section) {
            Some(value) => serde_yaml::to_string(value).map_err(|error| error.to_string())?,
            None => String::new(),
        }
    };
    Ok(json!({
        "path": rel,
        "section": section,
        "text": selected,
        "document": text,
        "nodes": nodes,
    }))
}

/// A flattened pre-order view of a YAML document: `path` is the key path to the
/// node, which is exactly what a write has to name.
fn collect_yaml_nodes(
    value: &serde_yaml::Value,
    path: &mut Vec<String>,
    depth: usize,
    out: &mut Vec<Value>,
) {
    let (kind, preview, children): (&str, String, Vec<(String, serde_yaml::Value)>) = match value {
        serde_yaml::Value::Mapping(mapping) => (
            "map",
            format!("{} keys", mapping.len()),
            mapping
                .iter()
                .filter_map(|(key, value)| key.as_str().map(|key| (key.to_owned(), value.clone())))
                .collect(),
        ),
        serde_yaml::Value::Sequence(items) => (
            "list",
            format!("{} items", items.len()),
            items
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value.clone()))
                .collect(),
        ),
        serde_yaml::Value::String(text) => ("string", one_line(text), Vec::new()),
        serde_yaml::Value::Bool(flag) => ("bool", flag.to_string(), Vec::new()),
        serde_yaml::Value::Number(number) => ("number", number.to_string(), Vec::new()),
        serde_yaml::Value::Null => ("null", "null".to_owned(), Vec::new()),
        other => ("other", format!("{other:?}"), Vec::new()),
    };
    out.push(json!({
        "path": path.clone(),
        "key": path.last().cloned().unwrap_or_default(),
        "depth": depth,
        "kind": kind,
        "preview": preview,
        "expandable": !children.is_empty(),
    }));
    for (key, child) in children {
        path.push(key);
        collect_yaml_nodes(&child, path, depth + 1, out);
        path.pop();
    }
}

fn one_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > 60 {
        format!("{}…", line.chars().take(60).collect::<String>())
    } else {
        line.to_owned()
    }
}

fn yaml_at<'a>(document: &'a serde_yaml::Value, section: &[String]) -> Option<&'a serde_yaml::Value> {
    let mut current = document;
    for key in section {
        current = current.get(key.as_str())?;
    }
    Some(current)
}

/// Write one YAML block into a container file, at the key path the user picked.
/// The container is stopped and started again when it is running, because a
/// plugin that cached its settings would otherwise keep serving the old ones.
pub(crate) fn enqueue_resource_write(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?
        .to_owned();
    let rel = request["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .ok_or("expected a container-relative path")?
        .to_owned();
    let section: Vec<String> = request["section"]
        .as_array()
        .map(|keys| keys.iter().filter_map(|key| key.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let text = request["text"].as_str().unwrap_or("").to_owned();
    let conflict = Conflict::parse(request["conflict"].as_str().unwrap_or("merge"))
        .ok_or("expected a conflict policy: refuse, merge or overwrite")?;
    let restart = request["restart"].as_bool().unwrap_or(true);
    if text.trim().is_empty() {
        return Err("expected the YAML to write".to_owned());
    }
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?
        .clone();
    let containers = state.containers.clone();
    let params = json!({
        "id": id,
        "path": rel,
        "section": section,
        "text": text,
        "conflict": request["conflict"].clone(),
    });
    enqueue_task_worker(
        state,
        "resource-write",
        vec![format!("container:{id}")],
        params,
        move |task| {
            let root = paths
                .runtime
                .clone()
                .ok_or("DSH Box storage is not configured")?;
            let container = container_dir(&root, &id)?;
            let is_running = containers
                .running
                .lock()
                .map_err(|_| "container registry lock failed".to_owned())?
                .contains_key(&id);
            if is_running && !restart {
                return Err(format!(
                    "container {id} is running; stop it first or allow a restart"
                ));
            }
            if is_running {
                task.update("Stopping the container", 30);
                stop_dsh_container(&id, &containers)?;
            }
            task.update("Writing the YAML block", 60);
            let written = transfer::write_section(&container, &rel, &section, &text, conflict)?;
            let label = if section.is_empty() {
                rel.clone()
            } else {
                format!("{rel}({})", section.join("."))
            };
            task.log(&format!(
                "{label}: {} bytes, {}",
                written.bytes,
                if written.replaced.is_empty() {
                    "added".to_owned()
                } else {
                    format!("replaced {}", written.replaced.join(", "))
                }
            ));
            task.check_cancelled()?;
            if is_running {
                task.update("Restarting the container", 90);
                start_dsh_container_inner(&id, &containers.running, Some(task))?;
            }
            Ok(())
        },
    )
    .map(HandlerResult::Async)
}

/// Every place a kind's state lives, in the form the transfer layer walks.
fn payload_parts_of(kind: &ResolvedKind) -> Vec<transfer::PayloadPart> {
    kind.payload_parts()
        .into_iter()
        .map(|part| transfer::PayloadPart {
            path: part.path,
            section: part.section,
        })
        .collect()
}

/// How a part list reads in a task log: `profile/settings.yaml(llm-pi-ai)`.
fn describe_parts(parts: &[transfer::PayloadPart]) -> String {
    parts
        .iter()
        .map(|part| {
            if part.section.is_empty() {
                part.path.clone()
            } else {
                format!("{}({})", part.path, part.section.join("."))
            }
        })
        .collect::<Vec<String>>()
        .join(", ")
}

/// The name a copy falls back to when the user gave none: the container's own
/// name, so `sessions` from `hello` and from `dshell` are two rows rather than
/// one that keeps being replaced.
fn container_name(root: &Path, id: &str) -> String {
    registered_containers(root)
        .unwrap_or_default()
        .into_iter()
        .find(|(container_id, _, _)| container_id == id)
        .map(|(_, name, _)| name)
        .unwrap_or_else(|| id.to_owned())
}

/// Taking a copy is an explicit act, so it adds one: the first free
/// `<base>`, `<base>-2`, `<base>-3`, … A user who wants to replace a copy
/// names the new one after it and removes the old one.
fn free_record_id(
    collection: &Collection<ResourceRecord>,
    base: &str,
) -> Result<String, String> {
    let taken: BTreeSet<String> = collection
        .load_all()?
        .into_iter()
        .map(|record| record.id)
        .collect();
    if !taken.contains(base) {
        return Ok(base.to_owned());
    }
    Ok((2u32..)
        .map(|nth| format!("{base}-{nth}"))
        .find(|id| !taken.contains(id))
        .unwrap_or_else(|| base.to_owned()))
}

fn profile_name(container: &Path) -> String {
    std::fs::read_to_string(container.join("container.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value["profile"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "web".to_owned())
}

fn records(state: &DaemonState) -> Result<Collection<ResourceRecord>, String> {
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    Ok(record::collection(box_store::open_document_store_for_paths(
        &paths,
    )?))
}

/// Everything a container could move: the built-in kinds, plugin declarations,
/// scanned candidates, and what has already been extracted.
pub(crate) fn list_container_resources(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?
        .to_owned();
    let plugin = request["plugin"].as_str().map(str::to_owned);
    let root = runtime_root(state)?;
    let container = container_dir(&root, &id)?;
    let profile = profile_name(&container);
    let resources = discover::discover(&container, &profile, plugin.as_deref());
    Ok(json!({
        "container": id,
        "profile": profile,
        "resources": resources,
        // Every installed package, so the picker offers what a bundle pulled
        // in as well as what the profile declares.
        "plugins": discover::installed_plugins(&container, &profile),
        "stored": records(state)?.load_all()?,
    }))
}

/// The extracted resources, independent of any container.
pub(crate) fn list_resources(state: &DaemonState, _request: &Value) -> Result<Value, String> {
    Ok(json!({ "resources": records(state)?.load_all()? }))
}

pub(crate) fn delete_resource(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["resourceId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a resource id")?
        .to_owned();
    let collection = records(state)?;
    let existing = collection
        .load_all()?
        .into_iter()
        .find(|record| record.id == id)
        .ok_or_else(|| format!("unknown resource: {id}"))?;
    let root = runtime_root(state)?;
    let directory = record::resources_root(&root).join(&id);
    if directory.is_dir() {
        std::fs::remove_dir_all(&directory)
            .map_err(|error| format!("cannot remove {}: {error}", directory.display()))?;
    }
    collection.remove(&id)?;
    Ok(json!({ "removed": id, "kind": existing.kind }))
}

/// The resource types the user pinned to the navigation.
pub(crate) fn list_resource_views(state: &DaemonState, _request: &Value) -> Result<Value, String> {
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let collection = record::view_collection(box_store::open_document_store_for_paths(&paths)?);
    let mut views = collection.load_all()?;
    views.sort_by(|left, right| left.created_at.cmp(&right.created_at));
    Ok(json!({ "views": views }))
}

/// Pin a resource type: the container is what the kind resolves against, and
/// the label is what the navigation shows.
pub(crate) fn add_resource_view(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let kind = request["kind"]
        .as_str()
        .filter(|kind| !kind.is_empty())
        .ok_or("expected a resource kind")?
        .to_owned();
    let container = request["container"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?
        .to_owned();
    let root = runtime_root(state)?;
    let directory = container_dir(&root, &container)?;
    let profile = profile_name(&directory);
    let dest = request["path"].as_str().filter(|path| !path.is_empty());
    let kind_info = resolve_kind(&directory, &profile, None, &kind, dest, None)?;
    let label = request["label"]
        .as_str()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(&kind_info.label)
        .to_owned();

    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let collection = record::view_collection(box_store::open_document_store_for_paths(&paths)?);
    // One view per (kind, container, path): adding the same type twice edits it.
    let id = record::build_id(&kind_info.id, &format!("{container}{}", dest.unwrap_or("")));
    let existing = collection
        .load_all()?
        .into_iter()
        .find(|view| view.id == id)
        .map(|view| view.created_at)
        .unwrap_or_else(now_seconds);
    let view = box_resources::ResourceView {
        id,
        label,
        kind: kind_info.id.clone(),
        container: container.clone(),
        path: dest.map(str::to_owned).or_else(|| Some(kind_info.path.clone())),
        secret: kind_info.secret,
        shape: kind_info.shape,
        entry_depth: kind_info.entry_depth,
        created_at: existing,
    };
    collection.upsert(&[view.clone()])?;
    Ok(json!({ "view": view }))
}

pub(crate) fn delete_resource_view(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["viewId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a view id")?
        .to_owned();
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let collection = record::view_collection(box_store::open_document_store_for_paths(&paths)?);
    collection.remove(&id)?;
    Ok(json!({ "removed": id }))
}

/// Everything of one resource type: where every container stands, and what has
/// been extracted. One call, so the type tab is a single round trip.
pub(crate) fn list_resource_type(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let kind = request["kind"]
        .as_str()
        .filter(|kind| !kind.is_empty())
        .ok_or("expected a resource kind")?
        .to_owned();
    let path = request["path"].as_str().filter(|path| !path.is_empty()).map(str::to_owned);
    let root = runtime_root(state)?;
    let mut containers = Vec::new();
    for (id, name, profile) in registered_containers(&root)? {
        let directory = root.join("instances").join(&id);
        let resolved = resolve_kind(&directory, &profile, None, &kind, path.as_deref(), None);
        let Ok(resolved) = resolved else {
            continue;
        };
        // A kind Box knows is measured through discovery; a path the user
        // pinned themselves is measured directly, or it would always read as
        // absent.
        let measured = discover::discover(&directory, &profile, None)
            .into_iter()
            .find(|entry| entry.kind.id == kind)
            .map(|entry| (entry.exists, entry.bytes, entry.files))
            .or_else(|| {
                let target = transfer::safe_join(&directory, &resolved.path).ok()?;
                let exists = target.exists();
                let (bytes, files) = if exists {
                    transfer::tree_stats(&target).unwrap_or((0, 0))
                } else {
                    (0, 0)
                };
                Some((exists, bytes, files))
            })
            .unwrap_or((false, 0, 0));
        containers.push(json!({
            "id": id,
            "name": name,
            "path": resolved.path,
            "secret": resolved.secret,
            "exists": measured.0,
            "bytes": measured.1,
            "files": measured.2,
        }));
    }
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let collection = record::collection(box_store::open_document_store_for_paths(&paths)?);
    let stored: Vec<ResourceRecord> = collection
        .load_all()?
        .into_iter()
        .filter(|record| record.kind == kind)
        .collect();
    Ok(json!({ "kind": kind, "containers": containers, "stored": stored }))
}

/// Every container on disk, as `(id, name, profile)` — read from the registry
/// rather than the running set, so a type view also covers stopped containers.
fn registered_containers(root: &Path) -> Result<Vec<(String, String, String)>, String> {
    let instances = root.join("instances");
    let mut containers = Vec::new();
    let Ok(entries) = std::fs::read_dir(&instances) else {
        return Ok(containers);
    };
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        if !entry.path().is_dir() || id.starts_with('.') {
            continue;
        }
        let container = entry.path();
        let name = std::fs::read_to_string(container.join("container.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value["name"].as_str().map(str::to_owned))
            .unwrap_or_else(|| id.clone());
        let profile = profile_name(&container);
        containers.push((id, name, profile));
    }
    containers.sort();
    Ok(containers)
}

/// List one directory of a container's storage area, so the UI can offer a
/// file tree instead of asking for a path. Read-only, and confined to the
/// container root — `safe_join` rejects anything that would leave it.
pub(crate) fn browse_container_paths(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?
        .to_owned();
    // Default to the profile, which is where resource state lives.
    let relative = request["path"].as_str().unwrap_or("profile").to_owned();
    let root = runtime_root(state)?;
    let container = container_dir(&root, &id)?;
    let directory = transfer::safe_join(&container, &relative)?;
    if !directory.is_dir() {
        return Err(format!("{relative} is not a directory in container {id}"));
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&directory).map_err(|error| format!("{}: {error}", directory.display()))? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir() && !meta.is_symlink();
        // Only files report a size here: a container holds trees big enough
        // that walking every directory would stall the panel.
        let bytes = if meta.is_file() { meta.len() } else { 0 };
        let children = if is_dir {
            std::fs::read_dir(&path).map(|dir| dir.count() as u64).unwrap_or(0)
        } else {
            0
        };
        entries.push(json!({
            "name": name,
            "path": format!("{relative}/{name}").replace("//", "/"),
            "directory": is_dir,
            "symlink": meta.is_symlink(),
            "bytes": bytes,
            "children": children,
            "secret": name.starts_with('.')
                || name.contains("credential")
                || matches!(path.extension().and_then(|ext| ext.to_str()), Some("key" | "pem" | "p12")),
        }));
    }
    entries.sort_by(|left, right| {
        let left_dir = left["directory"].as_bool().unwrap_or(false);
        let right_dir = right["directory"].as_bool().unwrap_or(false);
        right_dir
            .cmp(&left_dir)
            .then_with(|| left["name"].as_str().unwrap_or("").cmp(right["name"].as_str().unwrap_or("")))
    });
    let parent = Path::new(&relative)
        .parent()
        .map(|parent| parent.to_string_lossy().to_string())
        .filter(|parent| !parent.is_empty() && parent != ".")
        .unwrap_or_else(|| ".".to_owned());
    Ok(json!({ "container": id, "path": relative, "parent": parent, "entries": entries }))
}

/// Resolve a kind by id, preferring what this container knows about it, and
/// accepting an explicit `--dest` for a kind Box has never heard of.
fn resolve_kind(
    container: &Path,
    profile: &str,
    plugin: Option<&str>,
    kind_id: &str,
    dest: Option<&str>,
    declared: Option<ResolvedKind>,
) -> Result<ResolvedKind, String> {
    if let Some(kind) = declared {
        return Ok(kind);
    }
    let found = discover::discover(container, profile, plugin);
    if let Some(entry) = found.into_iter().find(|entry| {
        entry.kind.id == kind_id && dest.map_or(true, |dest| entry.kind.path == dest)
    }) {
        return Ok(entry.kind);
    }
    if let Some(path) = dest {
        let secret = box_resources::builtin(kind_id).is_some_and(|kind| kind.secret);
        return Ok(ResolvedKind::explicit(kind_id, path, secret));
    }
    Err(format!("unknown resource kind: {kind_id}"))
}

pub(crate) fn enqueue_resource_extract(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?
        .to_owned();
    let kind_id = request["kind"].as_str().unwrap_or("").to_owned();
    let entry = request["entry"].as_str().map(str::to_owned);
    let name = request["name"].as_str().map(str::to_owned);
    let plugin = request["plugin"].as_str().map(str::to_owned);
    let dest = request["dest"].as_str().map(str::to_owned);
    let out = request["out"].as_str().map(str::to_owned);
    if kind_id.is_empty() && dest.is_none() {
        return Err("expected a resource kind".to_owned());
    }
    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?
        .clone();
    let collection = records(state)?;
    let params = json!({
        "id": id,
        "kind": kind_id,
        "entry": entry,
        "name": name,
        "plugin": plugin,
    });
    enqueue_task_worker(
        state,
        "resource-extract",
        vec![format!("container:{id}")],
        params,
        move |task| {
            let root = paths
                .runtime
                .clone()
                .ok_or("DSH Box storage is not configured")?;
            let container = container_dir(&root, &id)?;
            let profile = profile_name(&container);
            task.update("Resolving the resource", 10);
            task.check_cancelled()?;

            let kind = resolve_kind(
                &container,
                &profile,
                plugin.as_deref(),
                &kind_id,
                dest.as_deref(),
                None,
            )?;
            // Naming the extraction after the entry keeps two sessions from
            // overwriting each other in the store; with neither a name nor an
            // entry it is named after the container it came out of, so the same
            // kind taken from two containers does not land on one row.
            let label = name
                .clone()
                .or_else(|| entry.clone().map(|entry| entry.replace('/', "-")))
                .unwrap_or_else(|| container_name(&root, &id));
            let record_id = free_record_id(&collection, &record::build_id(&kind.id, &label))?;
            let payload = record::payload_dir(&root, &record_id);

            task.update("Copying the payload", 40);
            // A kind can live in more than one place: a provider route is a
            // section of `settings.yaml` and its key is a ref in
            // `.credentials.yaml`, and neither half authenticates anything on
            // its own.
            let parts = payload_parts_of(&kind);
            let extracted = transfer::extract_parts(
                &container,
                &parts,
                entry.as_deref(),
                &payload,
                kind.secret,
            )?;
            task.log(&format!(
                "{} → {} ({} files, {} bytes)",
                describe_parts(&parts),
                payload.display(),
                extracted.files,
                extracted.bytes
            ));
            task.check_cancelled()?;

            let record = ResourceRecord {
                id: record_id.clone(),
                kind: kind.id.clone(),
                name: label,
                source_container: id.clone(),
                source_path: kind.path.clone(),
                digest: extracted.digest.clone(),
                bytes: extracted.bytes,
                files: extracted.files,
                secret: kind.secret,
                shape: kind.shape,
                entry_depth: kind.entry_depth,
                plugin: plugin.clone(),
                created_at: now_seconds(),
            };
            collection.upsert(&[record])?;
            task.update("Registering the resource", 80);

            if let Some(out) = out {
                transfer::pack_tar(&payload, Path::new(&out))?;
                task.log(&format!("packed {} ({})", out, extracted.digest));
            }
            Ok(())
        },
    )
    .map(HandlerResult::Async)
}

pub(crate) fn enqueue_resource_inject(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?
        .to_owned();
    let resource = request["resource"].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    let from = request["from"].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    let input = request["input"].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    let kind_id = request["kind"].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    let dest = request["dest"].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    let entry = request["entry"].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
    let conflict = Conflict::parse(request["conflict"].as_str().unwrap_or(""))
        .ok_or("conflict must be refuse, overwrite or merge")?;
    let restart = request["restart"].as_bool().unwrap_or(false);
    if resource.is_none() && from.is_none() && input.is_none() {
        return Err("expected a resource id, a source container, or a tarball".to_owned());
    }
    if let Some(entry) = &entry {
        if entry.contains("..") {
            return Err("entry must stay inside the resource".to_owned());
        }
    }

    let paths = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?
        .clone();
    let collection = records(state)?;
    let containers = state.containers.clone();
    let params = json!({
        "id": id,
        "resource": resource,
        "from": from,
        "input": input,
        "kind": kind_id,
        "dest": dest,
        "conflict": request["conflict"].clone(),
        "restart": restart,
    });

    enqueue_task_worker(
        state,
        "resource-inject",
        vec![format!("container:{id}")],
        params,
        move |task| {
            let root = paths
                .runtime
                .clone()
                .ok_or("DSH Box storage is not configured")?;
            let container = container_dir(&root, &id)?;
            let profile = profile_name(&container);
            task.update("Resolving the resource", 10);
            task.check_cancelled()?;

            // The payload: an extracted resource, another container's path, or
            // a tarball; the last two stage into a throwaway directory first.
            let mut staged: Option<PathBuf> = None;
            let (payload, record) = if let Some(resource) = &resource {
                let record = collection
                    .load_all()?
                    .into_iter()
                    .find(|entry| entry.id == *resource)
                    .ok_or_else(|| format!("unknown resource: {resource}"))?;
                let payload = record::payload_dir(&root, &record.id);
                if !payload.is_dir() {
                    return Err(format!("resource payload is gone: {}", payload.display()));
                }
                (payload, Some(record))
            } else if let Some(from) = &from {
                let source = container_dir(&root, from)?;
                let source_profile = profile_name(&source);
                let kind = resolve_kind(
                    &source,
                    &source_profile,
                    None,
                    kind_id.as_deref().unwrap_or("sessions"),
                    dest.as_deref(),
                    None,
                )?;
                let staging = staging_dir(&root, "inject");
                task.update("Reading the source container", 30);
                let extracted = transfer::extract(
                    &source,
                    &kind.path,
                    entry.as_deref(),
                    &staging,
                    kind.secret,
                )?;
                task.log(&format!(
                    "{from}: {} ({} files, {} bytes)",
                    kind.path, extracted.files, extracted.bytes
                ));
                staged = Some(staging.clone());
                (staging, None)
            } else {
                let input = input.clone().unwrap_or_default();
                let staging = staging_dir(&root, "inject");
                transfer::unpack_tar(Path::new(&input), &staging)?;
                task.log(&format!("unpacked {input}"));
                staged = Some(staging.clone());
                (staging, None)
            };

            // Where it goes: an explicit destination, the kind's path, or the
            // path the payload came from.
            let (path, shape, secret, depth) = match (&record, &kind_id, &dest) {
                (Some(record), _, dest) => (
                    dest.clone().unwrap_or_else(|| record.source_path.clone()),
                    record.shape,
                    record.secret,
                    record.entry_depth,
                ),
                (None, Some(kind_id), dest) => {
                    let kind = resolve_kind(
                        &container,
                        &profile,
                        None,
                        kind_id,
                        dest.as_deref(),
                        None,
                    )?;
                    (kind.path.clone(), kind.shape, kind.secret, kind.entry_depth)
                }
                (None, None, Some(dest)) => (dest.clone(), Shape::Opaque, false, 1),
                (None, None, None) => {
                    return Err("expected a resource kind or a destination".to_owned())
                }
            };

            // Writing into a live container is refused: plugins cache their
            // state, and a half-written session file is worse than a refusal.
            let is_running = containers
                .running
                .lock()
                .map_err(|_| "container registry lock failed".to_owned())?
                .contains_key(&id);
            if is_running && !restart {
                return Err(format!(
                    "container {id} is running; stop it first or pass restart"
                ));
            }
            if is_running {
                task.update("Stopping the container", 50);
                stop_dsh_container(&id, &containers)?;
            }

            task.update("Writing the payload", 70);
            // The payload says what it holds when it holds more than one part;
            // otherwise it is the single path this injection was told to use.
            let parts = transfer::payload_parts(&payload, &path);
            let injected = transfer::inject_parts(&payload, &container, &parts, conflict, shape, depth, secret)?;
            task.log(&format!(
                "{}: +{} files, replaced {}",
                describe_parts(&parts),
                injected.files,
                if injected.replaced.is_empty() {
                    "nothing".to_owned()
                } else {
                    injected.replaced.join(", ")
                }
            ));
            if let Some(staging) = staged {
                let _ = std::fs::remove_dir_all(staging);
            }
            task.check_cancelled()?;

            if is_running {
                task.update("Restarting the container", 90);
                start_dsh_container_inner(&id, &containers.running, Some(task))?;
            }
            Ok(())
        },
    )
    .map(HandlerResult::Async)
}

/// A throwaway directory for a payload that is being staged (a source
/// container or a tarball) — under the runtime so a large copy does not land
/// in `/tmp`.
fn staging_dir(root: &Path, purpose: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let directory = root
        .join("staging")
        .join(format!("{purpose}-{}-{stamp}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    directory
}

#[cfg(test)]
mod tests {
    use super::*;
    use box_resources::kinds::Shape;

    fn record(id: &str) -> ResourceRecord {
        ResourceRecord {
            id: id.to_owned(),
            kind: "sessions".to_owned(),
            name: id.to_owned(),
            source_container: "container-1".to_owned(),
            source_path: "profile/sessions".to_owned(),
            digest: "d".to_owned(),
            bytes: 1,
            files: 1,
            secret: false,
            shape: Shape::Entries,
            entry_depth: 2,
            plugin: None,
            created_at: 0,
        }
    }

    #[test]
    fn taking_a_copy_again_adds_one_instead_of_replacing_it() {
        let collection = Collection::memory("resources", record::record_key);
        assert_eq!(
            free_record_id(&collection, "sessions-hello").unwrap(),
            "sessions-hello"
        );
        collection.upsert(&[record("sessions-hello")]).unwrap();
        assert_eq!(
            free_record_id(&collection, "sessions-hello").unwrap(),
            "sessions-hello-2"
        );
        collection.upsert(&[record("sessions-hello-2")]).unwrap();
        assert_eq!(
            free_record_id(&collection, "sessions-hello").unwrap(),
            "sessions-hello-3"
        );
        // A different name is unaffected.
        assert_eq!(
            free_record_id(&collection, "credentials-hello").unwrap(),
            "credentials-hello"
        );
    }

    #[test]
    fn a_copy_without_a_name_is_named_after_its_container() {
        let root = std::env::temp_dir().join(format!("dshbox-resource-name-{}", std::process::id()));
        let container = root.join("instances").join("container-1");
        std::fs::create_dir_all(&container).unwrap();
        std::fs::write(
            container.join("container.json"),
            r#"{"id":"container-1","name":"hello"}"#,
        )
        .unwrap();
        assert_eq!(container_name(&root, "container-1"), "hello");
        // A container that is not registered yet still names the copy.
        assert_eq!(container_name(&root, "container-2"), "container-2");
        let _ = std::fs::remove_dir_all(root);
    }
}
