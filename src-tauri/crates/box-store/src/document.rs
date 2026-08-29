//! SQLite backend for the `DocumentStore` contract.

use box_foundation::collection::DocumentStore;
use box_foundation::BoxResult;
use rusqlite::{params, Connection};
use std::{
    path::Path,
    sync::Mutex,
    time::Duration,
};

/// Document schema, applied by migration v1. All domains share one table:
/// `scope` is the domain's collection name, `value` is the serialized
/// document. Domains that later need real SQL queries get their own typed
/// tables via a new migration — never by reaching into `value`.
const DOCUMENTS_SCHEMA_V1: &str = "\
CREATE TABLE IF NOT EXISTS documents (
    scope TEXT NOT NULL,
    id TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (scope, id)
);
CREATE INDEX IF NOT EXISTS idx_documents_scope ON documents(scope);";

/// Forward-only schema scripts. `MIGRATIONS[n]` brings `user_version == n`
/// up to `n + 1`. Each script runs inside one transaction, so a crash
/// mid-migration leaves the version at the last fully applied script and
/// the next open resumes from there.
const MIGRATIONS: &[&str] = &[DOCUMENTS_SCHEMA_V1];

/// SQLite-backed document store. The connection is wrapped in a mutex
/// because `rusqlite::Connection` is not `Sync`; the daemon's
/// thread-per-connection model means contention is bounded by task-worker
/// count. WAL journaling plus `busy_timeout` make the same database file
/// safe to share between the daemon and the desktop process.
pub struct SqliteDocumentStore {
    connection: Mutex<Connection>,
}

impl SqliteDocumentStore {
    pub fn open(path: &Path) -> BoxResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!("cannot create store directory {}: {error}", parent.display())
                })?;
            }
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("cannot open document store {}: {error}", path.display()))?;
        connection
            .busy_timeout(Duration::from_millis(5000))
            .map_err(|error| format!("cannot set document store busy timeout: {error}"))?;
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(|error| format!("cannot enable WAL on document store: {error}"))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(format!(
                "document store WAL mode unavailable (got {journal_mode}); \
                 the runtime directory may be on a filesystem that does not support it"
            ));
        }
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| format!("cannot set document store synchronous mode: {error}"))?;
        run_migrations(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
}

fn run_migrations(connection: &Connection) -> BoxResult<()> {
    let current: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| format!("cannot read store schema version: {error}"))?;
    for (offset, script) in MIGRATIONS.iter().enumerate() {
        let version = (offset + 1) as i64;
        if version <= current {
            continue;
        }
        connection
            .execute_batch(&format!("BEGIN;\n{script}\nCOMMIT;"))
            .map_err(|error| format!("store migration v{version} failed: {error}"))?;
        connection
            .pragma_update(None, "user_version", version)
            .map_err(|error| format!("cannot record store schema version {version}: {error}"))?;
    }
    Ok(())
}

const DOCUMENT_UPSERT: &str = "\
INSERT INTO documents (scope, id, value)
VALUES (?1, ?2, ?3)
ON CONFLICT(scope, id) DO UPDATE SET value = excluded.value";

impl DocumentStore for SqliteDocumentStore {
    fn load_scope(&self, scope: &str) -> BoxResult<Vec<(String, serde_json::Value)>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "document store lock failed".to_owned())?;
        let mut statement = connection
            .prepare("SELECT id, value FROM documents WHERE scope = ?1 ORDER BY id")
            .map_err(|error| format!("cannot read document store: {error}"))?;
        let documents = statement
            .query_map(params![scope], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("cannot read document store rows: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot read document store row: {error}"))?;
        documents
            .into_iter()
            .map(|(id, value)| {
                serde_json::from_str(&value)
                    .map(|parsed| (id.clone(), parsed))
                    .map_err(|error| {
                        format!("cannot decode document {scope}/{id}: {error}")
                    })
            })
            .collect()
    }

    fn put_documents(
        &self,
        scope: &str,
        documents: &[(String, serde_json::Value)],
    ) -> BoxResult<()> {
        if documents.is_empty() {
            return Ok(());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "document store lock failed".to_owned())?;
        let transaction = connection
            .transaction()
            .map_err(|error| format!("cannot begin document store transaction: {error}"))?;
        for (id, value) in documents {
            let encoded = serde_json::to_string(value)
                .map_err(|error| format!("cannot encode document {scope}/{id}: {error}"))?;
            transaction
                .execute(DOCUMENT_UPSERT, params![scope, id, encoded])
                .map_err(|error| format!("cannot put document {scope}/{id}: {error}"))?;
        }
        transaction
            .commit()
            .map_err(|error| format!("cannot commit document store transaction: {error}"))
    }

    fn delete_document(&self, scope: &str, id: &str) -> BoxResult<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "document store lock failed".to_owned())?;
        connection
            .execute(
                "DELETE FROM documents WHERE scope = ?1 AND id = ?2",
                params![scope, id],
            )
            .map_err(|error| format!("cannot delete document {scope}/{id}: {error}"))?;
        Ok(())
    }
}

/// Temp-directory helper shared by this crate's test modules.
#[cfg(test)]
pub(crate) mod tests_support {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    pub(crate) fn temp_dir(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let dir = std::env::temp_dir().join(format!("dsh-box-store-{name}-{millis}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::SqliteDocumentStore;
    use box_foundation::collection::{DocumentStore, JsonDocumentStore, MemoryDocumentStore};
    use crate::document::tests_support::temp_dir;
    use std::path::PathBuf;

    fn sqlite_store() -> SqliteDocumentStore {
        SqliteDocumentStore::open(&temp_dir("sqlite").join("state/dshbox.db")).unwrap()
    }

    #[test]
    fn sqlite_backend_satisfies_the_document_store_contract() {
        box_foundation::collection::test_kit::assert_document_store_contract(&mut || {
            Box::new(sqlite_store())
        });
    }

    #[test]
    fn json_backend_satisfies_the_document_store_contract() {
        box_foundation::collection::test_kit::assert_document_store_contract(&mut || {
            Box::new(JsonDocumentStore::new(temp_dir("json").join("state")))
        });
    }

    #[test]
    fn memory_backend_satisfies_the_document_store_contract() {
        box_foundation::collection::test_kit::assert_document_store_contract(&mut || {
            Box::new(MemoryDocumentStore::default())
        });
    }

    #[test]
    fn two_connections_share_one_database_without_lost_updates() {
        let path = temp_dir("shared").join("state/dshbox.db");
        let daemon = SqliteDocumentStore::open(&path).unwrap();
        let desktop = SqliteDocumentStore::open(&path).unwrap();
        daemon
            .put_documents("tasks", &[("daemon-task".to_owned(), serde_json::json!({"id": "daemon-task"}))])
            .unwrap();
        desktop
            .put_documents("tasks", &[("desktop-task".to_owned(), serde_json::json!({"id": "desktop-task"}))])
            .unwrap();
        // Each side sees both documents: neither put erased the other's row.
        assert_eq!(daemon.load_scope("tasks").unwrap().len(), 2);
        assert_eq!(desktop.load_scope("tasks").unwrap().len(), 2);
    }

    #[test]
    fn reopening_the_store_reuses_the_existing_schema() {
        let path = temp_dir("reopen").join("state/dshbox.db");
        let first = SqliteDocumentStore::open(&path).unwrap();
        first
            .put_documents("tasks", &[("persistent".to_owned(), serde_json::json!({"id": "persistent"}))])
            .unwrap();
        drop(first);
        let second = SqliteDocumentStore::open(&path).unwrap();
        assert_eq!(second.load_scope("tasks").unwrap().len(), 1);
    }
}
