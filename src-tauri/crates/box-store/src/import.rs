//! Store bootstrap for the daemon and desktop hosts: opens the SQLite
//! document store and migrates legacy per-domain JSON files on first use.

use box_foundation::collection::{read_legacy_documents, Collection, DocumentStore};
use box_foundation::{BoxPaths, BoxResult};
use box_scheduler::TaskRecord;
use std::{path::Path, sync::Arc};

/// Legacy per-domain JSON files that migrate into the document store on
/// first open. `tasks` is the pilot domain; later domains (sealed
/// templates, repository entries, resource map) append their scope here
/// when their migration ships.
const LEGACY_SCOPES: &[&str] = &["tasks"];

fn task_key(record: &TaskRecord) -> &str {
    record.id.as_str()
}

/// Open the SQLite document store for `paths`, importing legacy JSON scope
/// files (see `LEGACY_SCOPES`) on first use.
pub fn open_document_store_for_paths(paths: &BoxPaths) -> BoxResult<Arc<dyn DocumentStore>> {
    let store = crate::SqliteDocumentStore::open(&paths.store_db()?)?;
    import_legacy_scopes(&store, &state_dir(paths)?)?;
    Ok(Arc::new(store))
}

/// Open the task queue collection: SQLite-backed with legacy import. The
/// host injects this into `TaskManager::new`; on failure the host falls
/// back to `TaskManager::json` (daemon) or `TaskManager::memory` (desktop).
pub fn open_task_collection(paths: &BoxPaths) -> BoxResult<Collection<TaskRecord>> {
    Ok(Collection::new(
        open_document_store_for_paths(paths)?,
        "tasks",
        task_key,
    ))
}

fn state_dir(paths: &BoxPaths) -> BoxResult<std::path::PathBuf> {
    Ok(paths
        .runtime
        .as_ref()
        .ok_or("DSH Box storage is not configured")?
        .join("state"))
}

/// For every legacy scope file that exists: an empty store adopts its
/// documents; a populated store treats the file as superseded. Either way
/// the file is archived to `<scope>.json.pre-sqlite` so daemon and desktop
/// converge on the database. A file that no longer parses (a torn file from
/// the legacy non-atomic writer is the expected cause) is reported as an
/// error and left in place for the caller to decide: fall back to the file
/// backend or surface the failure.
fn import_legacy_scopes(store: &dyn DocumentStore, dir: &Path) -> BoxResult<()> {
    for scope in LEGACY_SCOPES {
        let json_path = dir.join(format!("{scope}.json"));
        if !json_path.exists() {
            continue;
        }
        let documents = read_legacy_documents(&json_path)?;
        if store.load_scope(scope)?.is_empty() && !documents.is_empty() {
            store.put_documents(scope, &documents)?;
        }
        let backup = json_path.with_file_name(format!("{scope}.json.pre-sqlite"));
        std::fs::rename(&json_path, &backup).map_err(|error| {
            format!(
                "cannot archive legacy {scope} store to {}: {error}",
                backup.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{open_task_collection, LEGACY_SCOPES};
    use crate::document::tests_support::temp_dir;
    use box_foundation::{BoxPaths, BoxResult};
    use box_scheduler::TaskRecord;

    fn paths(dir: &std::path::Path) -> BoxPaths {
        BoxPaths {
            config: dir.join("config.json"),
            runtime: Some(dir.to_path_buf()),
        }
    }

    fn legacy_record(id: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_owned(),
            kind: "test".to_owned(),
            resource_keys: vec![],
            status: "succeeded".to_owned(),
            stage: "Completed".to_owned(),
            progress: 100,
            created_at: 1_700_000_000,
            started_at: Some(1_700_000_050),
            finished_at: Some(1_700_000_100),
            log_path: "/tmp/unused.log".to_owned(),
            error: None,
            params: serde_json::json!({ "k": "v" }),
            cancel_requested: false,
            rollback_error: None,
        }
    }

    #[test]
    fn import_seeds_an_empty_store_and_archives_the_json_file() {
        let dir = temp_dir("import");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let legacy: Vec<TaskRecord> = vec![legacy_record("a"), legacy_record("b")];
        std::fs::write(
            state.join("tasks.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let collection = open_task_collection(&paths(&dir)).unwrap();
        let loaded = collection.load_all().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|task| task.id == "a"));
        assert!(!state.join("tasks.json").exists());
        assert!(state.join("tasks.json.pre-sqlite").exists());

        // Reopening keeps the imported rows (durability across restarts).
        drop(collection);
        let reopened = open_task_collection(&paths(&dir)).unwrap();
        assert_eq!(reopened.load_all().unwrap().len(), 2);
    }

    #[test]
    fn import_keeps_existing_rows_when_the_store_is_already_populated() {
        let dir = temp_dir("import-skip");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let legacy: Vec<TaskRecord> = vec![legacy_record("a"), legacy_record("b")];
        std::fs::write(
            state.join("tasks.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        // First open imports; write a fresh task; then a second legacy
        // file appears (another host that never migrated). Reopening must
        // keep the store's rows and archive the stale file, not re-import.
        let first = open_task_collection(&paths(&dir)).unwrap();
        first.upsert(&[legacy_record("live")]).unwrap();
        std::fs::write(
            state.join("tasks.json"),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();
        assert_eq!(LEGACY_SCOPES, ["tasks"]);
        let second = open_task_collection(&paths(&dir)).unwrap();
        let loaded = second.load_all().unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(loaded.iter().any(|task| task.id == "live"));
        assert!(!state.join("tasks.json").exists());
        assert!(state.join("tasks.json.pre-sqlite").exists());
    }

    #[test]
    fn import_reports_a_corrupt_file_and_leaves_it_in_place() {
        let dir = temp_dir("import-corrupt");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("tasks.json"), "{\"torn\": tru").unwrap();

        let error = match open_task_collection(&paths(&dir)) {
            Err(error) => error,
            Ok(_) => panic!("corrupt legacy file must fail the open"),
        };
        assert!(error.contains("cannot parse document file"), "{error}");
        assert!(state.join("tasks.json").exists());
        assert!(!state.join("tasks.json.pre-sqlite").exists());
    }

    #[test]
    fn import_is_a_no_op_without_a_legacy_file() {
        let dir = temp_dir("import-none");
        let collection = open_task_collection(&paths(&dir)).unwrap();
        assert!(collection.load_all().unwrap().is_empty());
    }
}
