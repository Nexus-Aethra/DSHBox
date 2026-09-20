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
            // overwriting each other in the store.
            let label = name
                .clone()
                .or_else(|| entry.clone().map(|entry| entry.replace('/', "-")))
                .unwrap_or_else(|| kind.id.clone());
            let record_id = record::build_id(&kind.id, &label);
            let payload = record::payload_dir(&root, &record_id);

            task.update("Copying the payload", 40);
            let extracted = transfer::extract(
                &container,
                &kind.path,
                entry.as_deref(),
                &payload,
                kind.secret,
            )?;
            task.log(&format!(
                "{} → {} ({} files, {} bytes)",
                kind.path,
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
            let injected = transfer::inject(&payload, &container, &path, conflict, shape, depth, secret)?;
            task.log(&format!(
                "{path}: +{} files, replaced {}",
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
