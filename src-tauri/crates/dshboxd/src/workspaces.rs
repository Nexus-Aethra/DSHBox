//! The DSH workspace registry a container persists, as a command surface.
//!
//! A workspace is a directory DSH will open sessions in. The UI adds one with a
//! native directory picker, and that picker is the reason this exists: it opens
//! on the host desktop, outside any page, so nothing that drives the app from
//! the outside can operate it. An agent that can only reach the UI can add a
//! workspace by clicking a button and then reaching for a file dialog it has no
//! way to answer.
//!
//! So the registry is read and written here instead. It is one JSON document at
//! `<container>/profile/storages/workspace.json`, and the same shape the UI
//! writes: a unit header, a global block, and a `workspaces` table keyed by id.
//! That last part is the reason this is a thin wrapper rather than a new store.
//! Sharing the file is what makes a workspace added here appear in the UI, and
//! vice versa -- a second registry would be a second truth.
//!
//! Writing is refused while the host is running. The host holds this document
//! in memory and rewrites it on its own schedule, so a write underneath it is
//! lost, silently, at the next save. Stopping first is the honest precondition
//! and the caller is told exactly which flag to pass.

use serde_json::{json, Value};
use uuid::Uuid;
use std::path::{Path, PathBuf};

/// The registry document's own version, as the document records it.
const REGISTRY_VERSION: u64 = 2;

/// The unit name DSH gives this document.
const UNIT_NAME: &str = "workspace";

/// Locate one container's workspace registry.
fn registry_path(container: &Path) -> PathBuf {
    container.join("profile").join("storages").join("workspace.json")
}

/// Read the registry, answering a first run with an empty one rather than an
/// error: a container that has never opened the UI has no file, and "no
/// workspaces" is the true answer, not a failure.
fn read_registry(path: &Path) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return empty_registry();
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(value) if value.get("tables").is_some() => value,
        // A document we cannot parse is not an empty one. Answering "no
        // workspaces" here would let the next add overwrite a registry the
        // user has data in, so this is an error the caller must see.
        _ => Value::Null,
    }
}

fn empty_registry() -> Value {
    json!({
        "unit": { "name": UNIT_NAME, "version": REGISTRY_VERSION },
        "global": { "initialized": true, "workspaceIds": [], "archivedSessionIds": [], "pinnedSessionIds": [] },
        "tables": { "workspaces": {} }
    })
}
/// Every workspace this container knows, newest first.
///
/// The ids are listed in `global.workspaceIds` as well as being table keys; the
/// UI reads that list for display order, so a registry that added a row without
/// adding the id would hold a workspace nothing shows.
pub(crate) fn list_workspaces(container: &Path) -> Result<Value, String> {
    let path = registry_path(container);
    let registry = read_registry(&path);
    if registry.is_null() {
        return Err(format!(
            "cannot read the workspace registry at {}: it is not the document Box writes; refusing to treat unreadable state as empty",
            path.display(),
        ));
    }
    let table = registry["tables"]["workspaces"].clone();
    let order: Vec<String> = registry["global"]["workspaceIds"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut rows = Vec::new();
    let Some(map) = table.as_object() else {
        return Ok(json!({ "workspaces": [] }));
    };
    // Order by the display list first, then anything the table holds but the
    // list forgot. A workspace missing from the list is a registry the UI
    // wrote in two halves; hiding it would be worse than showing it out of order.
    let mut ordered: Vec<&String> = order.iter().filter(|id| map.contains_key(id.as_str())).collect();
    let mut orphans: Vec<&String> = map.keys().filter(|id| !order.contains(id)).collect();
    orphans.sort();
    ordered.extend(orphans);
    for id in ordered {
        let Some(row) = map.get(id) else { continue };
        rows.push(json!({
            "id": id,
            "path": row["path"].as_str().unwrap_or_default(),
            "title": row["title"].as_str().unwrap_or_default(),
            "sessions": row["sessionIds"].as_array().map(Vec::len).unwrap_or(0),
            "listed": order.contains(id),
        }));
    }
    Ok(json!({ "workspaces": rows }))
}

/// The title DSH would give a directory: its last path segment.
///
/// Taken from the path with `Path` rather than by splitting on a separator, so
/// the last segment is the last segment on every platform -- a POSIX path does
/// not end in a backslash and a Windows path does not end in a forward slash.
fn default_title(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_owned()
}
/// Register a directory as a workspace.
///
/// Re-adding a path that is already registered returns that row with
/// `created: false` instead of failing. An agent setting up a session asks
/// "make this directory the workspace" as a normal step, and it has to be
/// able to ask it without first checking whether it already is -- otherwise the
/// idempotent case needs a separate list-and-compare, which is the kind of
/// ceremony a caller gets wrong.
pub(crate) fn add_workspace(container: &Path, path: &str, title: Option<&str>) -> Result<Value, String> {
    let requested = Path::new(path.trim());
    if path.trim().is_empty() {
        return Err("a workspace needs a path".to_owned());
    }
    if !requested.is_absolute() {
        // Box never guesses which directory a relative path was meant against:
        // the container is not the user's shell, so its working directory is
        // not a meaningful base. An absolute path is the only unambiguous one.
        return Err(format!(
            "{path:?} is not an absolute path; give the directory the platform's full path"
        ));
    }
    if !requested.is_dir() {
        return Err(format!(
            "{} is not a directory, so it cannot be a workspace",
            requested.display(),
        ));
    }
    let key = path.trim().to_owned();
    let registry_path = registry_path(container);
    let mut registry = read_registry(&registry_path);
    if registry.is_null() {
        return Err(format!(
            "cannot read the workspace registry at {}: it is not the document Box writes; refusing to overwrite unreadable state",
            registry_path.display(),
        ));
    }
    if registry["tables"]["workspaces"].is_null() {
        registry["tables"]["workspaces"] = json!({});
    }
    {
        let table = registry["tables"]["workspaces"]
            .as_object_mut()
            .ok_or_else(|| "the workspace registry's table is not an object".to_owned())?;
        for (id, row) in table.iter() {
            if row["path"].as_str() == Some(key.as_str()) {
                return Ok(json!({
                    "id": id,
                    "path": key,
                    "title": row["title"].as_str().unwrap_or(&default_title(&key)),
                    "created": false,
                }));
            }
        }
    }
    let id = new_workspace_id(&registry);
    let title = title
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| default_title(&key));
    let now = timestamp();
    registry["tables"]["workspaces"]
        .as_object_mut()
        .expect("just defaulted above")
        .insert(
            id.clone(),
            json!({
                "path": key,
                "title": title,
                "sessionIds": [],
                "createdAt": now,
                "updatedAt": now,
            }),
        );
    let listed = registry["global"]["workspaceIds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut ids = listed;
    ids.push(Value::String(id.clone()));
    registry["global"]["workspaceIds"] = Value::Array(ids);
    registry["unit"] = json!({ "name": UNIT_NAME, "version": REGISTRY_VERSION });
    registry["global"]["initialized"] = json!(true);
    write_registry(&registry_path, &registry)?;
    Ok(json!({ "id": id, "path": key, "title": title, "created": true }))
}
/// A workspace id that cannot collide with one already in the document.
///
/// The id is a random v4 UUID and collision is checked anyway: the cost is one
/// map lookup, and the alternative -- two rows sharing an id, one of them
/// silently unreachable -- is the kind of corruption nobody finds until a
/// workspace has gone missing.
fn new_workspace_id(registry: &Value) -> String {
    let taken = registry["tables"]["workspaces"]
        .as_object()
        .map(|table| table.keys().cloned().collect::<Vec<String>>())
        .unwrap_or_default();
    loop {
        let candidate = Uuid::new_v4().to_string();
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
}
/// An ISO-8601 instant, the format the registry's own rows carry.
fn timestamp() -> String {
    // Written by hand rather than pulled in: the document only needs a sortable
    // UTC stamp, and the calendar conversion is the whole of the work.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default();
    let (year, month, day, hour, minute, second) = civil_from_unix(now);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Days-to-civil, the standard inverse of Unix time, by Howard Hinnant's
/// algorithm. It shifts the year so a leap day lands at the end of the cycle
/// and counts from March, which is what makes the leap rule fall out with no
/// special case.
fn civil_from_unix(seconds: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        month,
        day,
        (rem / 3_600) as u32,
        (rem % 3_600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Write the registry back, creating its directory if this is the first one.
///
/// The write is a whole-file replace, which is what the host does with this
/// document too, and it is why the caller has to check the host is stopped
/// first: two writers each replacing the whole file means the slower one wins
/// with the other's rows already gone.
fn write_registry(path: &Path, registry: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(registry)
        .map_err(|error| format!("cannot encode the workspace registry: {error}"))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// `add_container_workspace` — register a directory, with the host stopped.
pub(crate) fn add_container_workspace(
    container: &Path,
    host_running: bool,
    request: &Value,
) -> Result<Value, String> {
    let path = request["path"].as_str().unwrap_or("").to_owned();
    if host_running {
        // Said here rather than inferred: the symptom otherwise is a workspace
        // that vanished some minutes later, with no error anywhere near the
        // moment it was added.
        return Err(
            String::from(
                "this container is running, and the running host rewrites its own "
            )
            + "workspace registry; stop it first (dshbox container stop <id>), "
            + "or pass --restart to do both in order",
        );
    }    let title = request["title"].as_str();
    add_workspace(container, &path, title)
}
/// Resolve one container's directory the way the daemon stores it.
fn container_dir(state: &crate::state::DaemonState, id: &str) -> Result<std::path::PathBuf, String> {
    let root = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?
        .runtime
        .clone()
        .ok_or_else(|| "DSH Box storage is not configured".to_owned())?;
    let directory = root.join("instances").join(id);
    if directory.is_dir() {
        Ok(directory)
    } else {
        Err(format!("container not found: {id}"))
    }
}

/// Whether this container's host process is alive right now.
///
/// Read from the durable host record and checked against the pid table, not
/// from the in-memory registry: the same reason `container_url_probe` does it
/// that way. A container the daemon has just been told about still has to be
/// answered correctly.
fn host_is_running(id: &str) -> bool {
    crate::dispatch::container_url_probe(id).is_ok()
}

/// `list_container_workspaces` — every workspace this container would show.
pub(crate) fn list_container_workspaces_rpc(
    state: &crate::state::DaemonState,
    request: &Value,
) -> Result<Value, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?;
    list_workspaces(&container_dir(state, id)?)
}

/// `add_container_workspace` — register a directory, host stopped.
pub(crate) fn add_container_workspace_rpc(
    state: &crate::state::DaemonState,
    request: &Value,
) -> Result<Value, String> {
    let id = request["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or("expected a container id")?;
    let directory = container_dir(state, id)?;
    add_container_workspace(&directory, host_is_running(id), request)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// A container whose profile directory exists, with a directory to register.
    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("dshbox-ws-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let target = root.join("work");
        std::fs::create_dir_all(&target).unwrap();
        (root, target)
    }

    #[test]
    fn a_fresh_container_has_no_workspaces_and_is_not_an_error() {
        let (root, _) = scratch("fresh");
        let out = list_workspaces(&root).unwrap();
        assert_eq!(out["workspaces"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn adding_registers_the_path_and_lists_it_once() {
        let (root, target) = scratch("add");
        let added = add_workspace(&root, &to_str(&target), None).unwrap();
        assert_eq!(added["created"], json!(true));
        let listed = list_workspaces(&root).unwrap();
        let rows = listed["workspaces"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{listed}");
        assert_eq!(rows[0]["path"], json!(to_str(&target)));
        // The title is the last path segment, which is the whole point of
        // deriving it: a separator-agnostic name on every platform.
        assert_eq!(rows[0]["title"], json!("work"));
    }

    #[test]
    fn adding_the_same_path_twice_is_reported_not_rejected() {
        let (root, target) = scratch("twice");
        let first = add_workspace(&root, &to_str(&target), None).unwrap();
        let second = add_workspace(&root, &to_str(&target), None).unwrap();
        assert_eq!(first["created"], json!(true));
        assert_eq!(second["created"], json!(false));
        assert_eq!(first["id"], second["id"]);
        assert_eq!(
            list_workspaces(&root).unwrap()["workspaces"].as_array().unwrap().len(),
            1,
        );
    }

    #[test]
    fn a_relative_path_is_refused_rather_than_resolved() {
        let (root, _) = scratch("relative");
        let error = add_workspace(&root, "work", None).unwrap_err();
        assert!(error.contains("absolute"), "{error}");
    }

    #[test]
    fn a_missing_directory_is_refused() {
        let (root, _) = scratch("missing");
        let error = add_workspace(&root, &to_str(&root.join("nope")), None).unwrap_err();
        assert!(error.contains("not a directory"), "{error}");
    }

    #[test]
    fn an_unreadable_registry_is_refused_rather_than_treated_as_empty() {
        let (root, target) = scratch("corrupt");
        let path = registry_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let error = add_workspace(&root, &to_str(&target), None).unwrap_err();
        assert!(error.contains("refusing"), "{error}");
    }

    #[test]
    fn a_row_the_display_list_omits_is_still_reported() {
        let (root, target) = scratch("orphan");
        let path = registry_path(&root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            json!({
                "unit": { "name": "workspace", "version": 2 },
                "global": { "initialized": true, "workspaceIds": [] },
                "tables": { "workspaces": {
                    "orphaned": { "path": to_str(&target), "title": "work", "sessionIds": [] }
                } }
            })
            .to_string(),
        )
        .unwrap();
        let listed = list_workspaces(&root).unwrap();
        let rows = listed["workspaces"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{listed}");
        assert_eq!(rows[0]["listed"], json!(false));
    }

    #[test]
    fn civil_conversion_agrees_with_known_instants() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // A leap day, which is the case a hand-rolled conversion gets wrong.
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29, 0, 0, 0));
        assert_eq!(civil_from_unix(1_735_689_599), (2024, 12, 31, 23, 59, 59));
    }

    fn to_str(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }
}
