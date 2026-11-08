//! Generic document-store persistence: one backend contract shared by every
//! domain, plus a typed collection wrapper.
//!
//! Before this module, each persisted index (task queue, sealed templates,
//! repository entries, …) owned a bespoke read-modify-write JSON file. The
//! document layer replaces that pattern: a domain declares a `scope`, stores
//! `(id, JSON)` documents, and the backend (SQLite today, JSON for legacy
//! layouts and tests, memory for unconfigured hosts) owns durability,
//! transactions, and cross-process safety once for all domains.
//!
//! Domains that later outgrow CRUD (queries, reconciliation) add a typed
//! repository trait for themselves; they must not poke at backend SQL.

use crate::BoxResult;
use serde::{de::DeserializeOwned, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Backend contract: documents grouped into named scopes, keyed by id.
///
/// `put_documents` must insert-or-replace by id and must never delete ids
/// absent from the batch — the daemon and the desktop share one store, and
/// a whole-scope rewrite from either side would erase the other's rows.
/// Deletion goes exclusively through `delete_document`.
pub trait DocumentStore: Send + Sync {
    /// Load every `(id, document)` pair in `scope`. Empty or missing scopes
    /// yield `vec![]`.
    fn load_scope(&self, scope: &str) -> BoxResult<Vec<(String, serde_json::Value)>>;
    /// Insert or replace each document by id, durably, before returning.
    fn put_documents(&self, scope: &str, documents: &[(String, serde_json::Value)])
        -> BoxResult<()>;
    /// Delete a single document by id. Deleting an unknown id is not an error.
    fn delete_document(&self, scope: &str, id: &str) -> BoxResult<()>;
}

/// In-process backend for tests and for hosts with no configured runtime.
#[derive(Default)]
pub struct MemoryDocumentStore {
    scopes: Mutex<BTreeMap<String, BTreeMap<String, serde_json::Value>>>,
}

impl DocumentStore for MemoryDocumentStore {
    fn load_scope(&self, scope: &str) -> BoxResult<Vec<(String, serde_json::Value)>> {
        let scopes = self
            .scopes
            .lock()
            .map_err(|_| "document store lock failed".to_owned())?;
        Ok(scopes
            .get(scope)
            .map(|documents| {
                documents
                    .iter()
                    .map(|(id, value)| (id.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn put_documents(
        &self,
        scope: &str,
        documents: &[(String, serde_json::Value)],
    ) -> BoxResult<()> {
        let mut scopes = self
            .scopes
            .lock()
            .map_err(|_| "document store lock failed".to_owned())?;
        let target = scopes.entry(scope.to_owned()).or_default();
        for (id, value) in documents {
            target.insert(id.clone(), value.clone());
        }
        Ok(())
    }

    fn delete_document(&self, scope: &str, id: &str) -> BoxResult<()> {
        if let Some(documents) = self
            .scopes
            .lock()
            .map_err(|_| "document store lock failed".to_owned())?
            .get_mut(scope)
        {
            documents.remove(id);
        }
        Ok(())
    }
}

/// File backend: one JSON object file per scope at `<root>/<scope>.json`,
/// mapping id → document. This is also the migration reader for the legacy
/// per-domain files (the task queue's `tasks.json` was a plain array of
/// records; see `read_legacy_documents`).
pub struct JsonDocumentStore {
    root: PathBuf,
}

impl JsonDocumentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, scope: &str) -> PathBuf {
        self.root.join(format!("{scope}.json"))
    }

    fn read_file(&self, scope: &str) -> BoxResult<Option<serde_json::Value>> {
        let path = self.path(scope);
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
        serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| format!("cannot parse document store {}: {error}", path.display()))
    }

    fn write_file(&self, scope: &str, documents: &BTreeMap<String, serde_json::Value>) -> BoxResult<()> {
        let path = self.path(scope);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(
            &path,
            serde_json::to_string_pretty(documents).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }
}

impl DocumentStore for JsonDocumentStore {
    fn load_scope(&self, scope: &str) -> BoxResult<Vec<(String, serde_json::Value)>> {
        match self.read_file(scope)? {
            None => Ok(Vec::new()),
            Some(serde_json::Value::Object(map)) => {
                Ok(map.into_iter().collect())
            }
            Some(_) => Err(format!(
                "document store {} must hold a JSON object mapping id → document",
                self.path(scope).display()
            )),
        }
    }

    fn put_documents(&self, scope: &str, documents: &[(String, serde_json::Value)]) -> BoxResult<()> {
        if documents.is_empty() {
            return Ok(());
        }
        let mut stored = match self.read_file(scope)? {
            None => BTreeMap::new(),
            Some(serde_json::Value::Object(map)) => map.into_iter().collect(),
            Some(_) => {
                return Err(format!(
                    "document store {} must hold a JSON object mapping id → document",
                    self.path(scope).display()
                ))
            }
        };
        for (id, value) in documents {
            stored.insert(id.clone(), value.clone());
        }
        self.write_file(scope, &stored)
    }

    fn delete_document(&self, scope: &str, id: &str) -> BoxResult<()> {
        if !self.path(scope).exists() {
            return Ok(());
        }
        let mut stored = match self.read_file(scope)? {
            None => return Ok(()),
            Some(serde_json::Value::Object(map)) => map.into_iter().collect::<BTreeMap<_, _>>(),
            Some(_) => return Ok(()),
        };
        if stored.remove(id).is_some() {
            self.write_file(scope, &stored)?;
        }
        Ok(())
    }
}

/// Extract `(id, document)` pairs from a legacy per-domain file. Legacy
/// layouts were either a JSON array of objects carrying an `"id"` field
/// (`tasks.json`) or an id-keyed object; both are accepted so the migration
/// into a `DocumentStore` needs no per-domain code.
pub fn read_legacy_documents(path: &Path) -> BoxResult<Vec<(String, serde_json::Value)>> {
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("cannot parse document file {}: {error}", path.display()))?;
    match value {
        serde_json::Value::Array(records) => records
            .into_iter()
            .map(|record| {
                let id = record["id"].as_str().ok_or_else(|| {
                    format!(
                        "legacy document file {} holds a record without an id",
                        path.display()
                    )
                })?;
                Ok((id.to_owned(), record))
            })
            .collect(),
        serde_json::Value::Object(map) => Ok(map.into_iter().collect()),
        _ => Err(format!(
            "legacy document file {} must hold an array or an id-keyed object",
            path.display()
        )),
    }
}

/// A strongly-typed view of one scope: `T` values keyed by `key(&T)`.
/// Cloneable; clones share the backend.
pub struct Collection<T> {
    store: std::sync::Arc<dyn DocumentStore>,
    scope: String,
    key: fn(&T) -> &str,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T: Serialize + DeserializeOwned + Clone> Clone for Collection<T> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            scope: self.scope.clone(),
            key: self.key,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T: Serialize + DeserializeOwned + Clone> Collection<T> {
    pub fn new(
        store: std::sync::Arc<dyn DocumentStore>,
        scope: impl Into<String>,
        key: fn(&T) -> &str,
    ) -> Self {
        Self {
            store,
            scope: scope.into(),
            key,
            _marker: std::marker::PhantomData,
        }
    }

    /// In-memory collection — for hosts with no configured runtime and for
    /// tests that never cross manager instances.
    pub fn memory(scope: impl Into<String>, key: fn(&T) -> &str) -> Self {
        Self::new(
            std::sync::Arc::new(MemoryDocumentStore::default()),
            scope,
            key,
        )
    }

    /// File-backed collection under `root` (`<root>/<scope>.json`).
    pub fn json_dir(root: impl Into<PathBuf>, scope: impl Into<String>, key: fn(&T) -> &str) -> Self {
        Self::new(std::sync::Arc::new(JsonDocumentStore::new(root)), scope, key)
    }

    pub fn load_all(&self) -> BoxResult<Vec<T>> {
        self.store
            .load_scope(&self.scope)?
            .into_iter()
            .map(|(id, value)| {
                serde_json::from_value(value)
                    .map_err(|error| format!("cannot decode {} document {id}: {error}", self.scope))
            })
            .collect()
    }

    /// Insert or replace each item by its key, durably, before returning.
    pub fn upsert(&self, items: &[T]) -> BoxResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        let documents = items
            .iter()
            .map(|item| {
                let id = (self.key)(item);
                let value = serde_json::to_value(item)
                    .map_err(|error| format!("cannot encode {0} document: {error}", self.scope))?;
                Ok((id.to_owned(), value))
            })
            .collect::<BoxResult<Vec<_>>>()?;
        self.store.put_documents(&self.scope, &documents)
    }

    /// Delete a single item by key.
    pub fn remove(&self, id: &str) -> BoxResult<()> {
        self.store.delete_document(&self.scope, id)
    }
}

/// Shared behavioral contract for `DocumentStore` backends, exercised by
/// every implementation's test suite so a new backend cannot drift.
#[cfg(feature = "test-kit")]
pub mod test_kit {
    use super::DocumentStore;

    fn sample(id: &str, status: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": "container-start",
            "status": status,
            "params": { "list": [1, 2, 3] },
        })
    }

    /// Run the full store contract against a fresh backend per scenario.
    pub fn assert_document_store_contract(
        make_store: &mut dyn FnMut() -> Box<dyn DocumentStore>,
    ) {
        // Empty store loads empty.
        assert!(make_store().load_scope("tasks").unwrap().is_empty());

        // Round-trip preserves documents; scopes are independent.
        {
            let store = make_store();
            store
                .put_documents("tasks", &[("a".to_owned(), sample("a", "queued"))])
                .unwrap();
            store
                .put_documents("containers", &[("a".to_owned(), sample("a", "running"))])
                .unwrap();
            let tasks = store.load_scope("tasks").unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].1["status"], "queued");
            assert_eq!(store.load_scope("containers").unwrap()[0].1["status"], "running");
        }

        // Putting the same id replaces instead of duplicating.
        {
            let store = make_store();
            store
                .put_documents("tasks", &[("a".to_owned(), sample("a", "queued"))])
                .unwrap();
            store
                .put_documents("tasks", &[("a".to_owned(), sample("a", "running"))])
                .unwrap();
            let loaded = store.load_scope("tasks").unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].1["status"], "running");
        }

        // Putting one id leaves unrelated ids and scopes intact.
        {
            let store = make_store();
            store
                .put_documents(
                    "tasks",
                    &[("a".to_owned(), sample("a", "queued")), ("b".to_owned(), sample("b", "queued"))],
                )
                .unwrap();
            store
                .put_documents("tasks", &[("b".to_owned(), sample("b", "running"))])
                .unwrap();
            let loaded = store.load_scope("tasks").unwrap();
            assert_eq!(loaded.len(), 2);
            assert_eq!(
                loaded.iter().find(|(id, _)| id == "a").unwrap().1["status"],
                "queued"
            );
        }

        // delete_document removes exactly its id and tolerates unknown ids.
        {
            let store = make_store();
            store
                .put_documents(
                    "tasks",
                    &[("a".to_owned(), sample("a", "queued")), ("b".to_owned(), sample("b", "queued"))],
                )
                .unwrap();
            store.delete_document("tasks", "a").unwrap();
            store.delete_document("tasks", "missing").unwrap();
            let loaded = store.load_scope("tasks").unwrap();
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].0, "b");
        }

        // A batch put lands every document.
        {
            let store = make_store();
            let documents: Vec<_> = (0..5)
                .map(|index| (format!("t{index}"), sample(&format!("t{index}"), "queued")))
                .collect();
            store.put_documents("tasks", &documents).unwrap();
            assert_eq!(store.load_scope("tasks").unwrap().len(), 5);
        }
    }
}
