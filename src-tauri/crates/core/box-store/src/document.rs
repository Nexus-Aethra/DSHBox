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
        // The database is where secrets land: node bootstrap keys, resource
        // credentials, task params. A file the whole machine can read makes
        // "never log a secret" a half-measure -- the secret is still sitting
        // in a world-readable file in the runtime directory. Tighten it on
        // every open, not only on create, because an earlier build may have
        // left it loose.
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
        // Tighten after the migrations, not before: the migrations are the
        // first write, and the first write is what creates the -wal and -shm
        // sidecars. Tightening before it would find only the main file.
        tighten_store_permissions(path);
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Open the secrets store: same schema, no WAL, tightened on create.
    ///
    /// The difference from `open` is the journal mode, and it is the whole
    /// point. WAL keeps the real bytes in a `-wal` sidecar and leaves the main
    /// file a near-empty header, so "this file is 0600" stops describing where
    /// the data is -- a review of PR #51 proved it by reading the bytes back
    /// and found the PSK and the control-plane private key entirely in the
    /// sidecar, which SQLite had created with the default umask. With the
    /// rollback journal instead, the data is in the one file we chmod, and
    /// there is nothing to miss.
    ///
    /// The trade is concurrency: the rollback journal allows one writer at a
    /// time, where WAL allows a writer beside readers. That is fine here and
    /// it is the wrong trade for the main store, which the daemon and the
    /// desktop share -- hence two stores rather than one with a flag.
    pub fn open_secrets(path: &Path) -> BoxResult<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!("cannot create secrets directory {}: {error}", parent.display())
                })?;
            }
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("cannot open secrets store {}: {error}", path.display()))?;
        connection
            .busy_timeout(Duration::from_millis(5000))
            .map_err(|error| format!("cannot set secrets store busy timeout: {error}"))?;
        // The rollback journal, not WAL. See the doc comment: this is what
        // keeps the bytes in the file we tighten.
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .map_err(|error| format!("cannot set secrets store journal mode: {error}"))?;
        if !journal_mode.eq_ignore_ascii_case("delete") {
            return Err(format!("secrets store journal mode unavailable (got {journal_mode})"));
        }
        // FULL rather than NORMAL: NORMAL is the WAL-appropriate durability
        // level, and with the rollback journal it would skip the fsync that
        // makes a committed secret survive a crash.
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|error| format!("cannot set secrets store synchronous mode: {error}"))?;
        run_migrations(&connection)?;
        // No ordering subtlety here: the rollback journal leaves no sidecars
        // behind, so tightening after the migrations and tightening before
        // them are the same call.
        tighten_store_permissions(path);
        // On Windows the mode mask is not the control -- inheritance is, so a
        // chmod-equivalent does nothing and the DACL has to be replaced. This
        // is the only place in the store where a failure to tighten is fatal:
        // the main store falls back to best-effort because a store that cannot
        // be read is worse than one that is loose, but a secrets store whose
        // protection was refused is exactly the state PR #51 was about, and
        // continuing would write the next enrolment into a file we just failed
        // to protect.
        #[cfg(windows)]
        if let Err(code) = restrict_to_owner(path) {
            return Err(format!(
                "cannot restrict the secrets store {} (win32 error {code}): \
                 refusing to store bootstrap secrets in a file other local users can read",
                path.display()
            ));
        }
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

/// Tighten the database and its WAL sidecars to owner-only.
///
/// The sidecars are the point, and getting this wrong is silent: the store
/// opens with `journal_mode=WAL`, so **every write lands in `-wal` first**
/// and the main file stays a near-empty header. A review proved it by reading
/// the bytes back -- the main `.db` held nothing, the `-wal` held the PSK and
/// the control-plane private key. Tightening only the `.db` is tightening a
/// file with no secrets in it.
///
/// All three files are touched, not just the ones that exist right now:
/// SQLite creates `-wal` and `-shm` lazily, so a read-only open may not have
/// produced them yet and the first write would create them at the default
/// umask. The store is opened before any write here, so the set is what
/// exists at open time; a file created later is caught by the next open.
///
/// Called on every open rather than only on create: a store written by an
/// earlier build may have been left world-readable, and the first time this
/// code runs it should repair that rather than inherit it. Best-effort by
/// design -- a filesystem without Unix permissions (or a Windows volume) has
/// nothing to tighten, and failing the open there would make the store
/// unusable on platforms where the file ACL is the only control.
fn tighten_store_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for candidate in store_files(path) {
            if let Ok(metadata) = std::fs::metadata(&candidate) {
                let mut permissions = metadata.permissions();
                if permissions.mode() & 0o077 != 0 {
                    permissions.set_mode(0o600);
                    let _ = std::fs::set_permissions(&candidate, permissions);
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// The database and the sidecars SQLite may have created beside it.
///
/// `-wal` is the write-ahead log, which is where the bytes actually are under
/// WAL; `-shm` is the shared-memory index for it. Both are created by SQLite
/// with the process umask, so both need the same tightening.
#[cfg(unix)]
fn store_files(path: &Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        out.push(std::path::PathBuf::from(name));
    }
    out
}




// ---------- Windows: an explicit DACL ----------

/// Give a file a DACL naming only SYSTEM, CREATOR OWNER, and Administrators.
///
/// The reason this exists is what a fresh Windows install *inherits*. A
/// review of PR #51 read the ACL back off a real runtime directory:
///
///   NT AUTHORITY\Authenticated Users : Modify, Synchronize
///   BUILTIN\Users                     : ReadAndExecute, Synchronize
///
/// both inherited from the parent directory. On a multi-user machine that is
/// every interactive user holding read -- and for Modify, write -- on the file
/// with the control-plane Ed25519 private key. The Unix path tightens a mode
/// mask; this replaces the whole DACL, because inheritance is the thing
/// granting the access.
///
/// PROTECTED_DACL_SECURITY_INFORMATION is the load-bearing flag: without it the
/// written DACL is only an additional grant and the parent directory keeps
/// granting everything it grants now.
///
/// Returns the Win32 error code rather than a bool, because the caller has to
/// decide what a failure means and the interesting cases differ:
/// ACCESS_DENIED means the daemon cannot fix its own file, which is worth
/// saying out loud, and anything else means this code is wrong.
#[cfg(windows)]
fn restrict_to_owner(path: &Path) -> Result<(), u32> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSidToSidW, SetEntriesInAclW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
        TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION};
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};

    // Three well-known SIDs: SY is SYSTEM, CO is CREATOR OWNER, BA is
    // BUILTIN\Administrators.
    //
    // CO is used as a spelling, not for the resilience an earlier comment
    // claimed for it. On a file CREATOR OWNER resolves to the owner SID at
    // write time -- which is the running user, i.e. exactly the snapshot that
    // comment said it was avoiding. It only stays abstract when inherited,
    // which is the container case and not this one. So the grant is whoever
    // ran the daemon when the file was written.
    //
    // Granting the token user SID directly instead would be more honest and
    // would let an operator switch the daemon to a service account without a
    // manual ACL edit. That needs OpenProcessToken and GetTokenInformation,
    // which is a bigger change than this commit and belongs with the review
    // that asks for service-account support.
    let mut trustees: Vec<TRUSTEE_W> = Vec::new();
    let mut sids: Vec<*mut core::ffi::c_void> = Vec::new();
    for name in ["SY", "CO", "BA"] {
        let mut sid: *mut core::ffi::c_void = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSidToSidW(wide(name).as_ptr(), &mut sid as *mut *mut core::ffi::c_void)
        };
        if ok == 0 {
            return Err(unsafe { windows_sys::Win32::Foundation::GetLastError() });
        }
        sids.push(sid);
        trustees.push(TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        });
    }

    let access: Vec<EXPLICIT_ACCESS_W> = trustees
        .into_iter()
        .map(|trustee| EXPLICIT_ACCESS_W {
            // GENERIC_ALL. The file is the daemon own; nobody else needs a
            // narrower grant, and enumerating the rights each principal needs
            // is how one of them ends up unable to read its own key.
            grfAccessPermissions: 0x1000_0000,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: trustee,
        })
        .collect();

    let mut acl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
    let built = unsafe {
        SetEntriesInAclW(
            access.len() as u32,
            access.as_ptr(),
            std::ptr::null_mut(),
            &mut acl,
        )
    };
    if built != 0 {
        let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        for sid in sids {
            unsafe { LocalFree(sid) };
        }
        return Err(error);
    }

    let wide_path = wide(path);
    let applied = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null_mut(),
        )
    };
    let result = if applied == 0 {
        Ok(())
    } else {
        Err(unsafe { windows_sys::Win32::Foundation::GetLastError() })
    };

    // Free what ConvertStringSidToSidW allocated. The ACL is owned by the file
    // now, so it is not freed here.
    for sid in sids {
        unsafe { LocalFree(sid) };
    }
    result
}

/// A null-terminated wide string, for the Win32 APIs that take one.

// ---------- Windows: read the DACL back ----------

/// The DACL of a file as an SDDL string.
///
/// SDDL is the textual form of a security descriptor, and going through it is
/// deliberate: the previous two implementations walked the ACL bytes by hand
/// and the second one corrupted the heap -- the tests passed and the process
/// died with STATUS_HEAP_CORRUPTION, which is what an off-by-one in a
/// structure the compiler does not check looks like. A string has no such
/// offsets, and the assertions become substring checks that cannot overread.
///
/// An empty DACL is rendered as "D:" with nothing after it, which the caller
/// treats as no grants rather than as a pass.
// Not dead: used by the Windows ACL tests below, which `cargo build` on other
// platforms does not compile. Deleting this on the strength of the warning is
// how the ACL tests lose their readback.
#[cfg(windows)]
#[allow(dead_code)]
fn dacl_sddl(path: &Path) -> Result<String, u32> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    };

    let wide_path = wide(path);
    let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
    // The whole descriptor, not just the DACL: the string form is built from
    // a security descriptor, and handing it a bare ACL is a revision
    // mismatch (win32 error 1305).
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(status as u32);
    }

    // Revision 1 is the current SDDL revision; the flag asks for the DACL
    // portion only, which is the part under test.
    let mut text: *mut u16 = std::ptr::null_mut();
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            1,
            DACL_SECURITY_INFORMATION as u32,
            &mut text,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        return Err(error);
    }
    let mut len = 0usize;
    unsafe {
        while *text.add(len) != 0 {
            len += 1;
        }
    }
    let sddl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    unsafe { LocalFree(text as *mut core::ffi::c_void) };
    Ok(sddl)
}

/// The accounts a secrets file must not grant to, as SDDL fragments.
///
/// These are what a fresh Windows install inherits from the parent directory,
/// and the reason the DACL is replaced rather than tightened:
/// NT AUTHORITY\Authenticated Users and BUILTIN\Users between them cover every
/// interactive user on the machine, so the file with the control-plane Ed25519
/// private key would be readable by all of them.
///
/// Each entry carries the SDDL abbreviation rather than the raw SID, because
/// ConvertSecurityDescriptorToStringSecurityDescriptorW emits the
/// abbreviation for a well-known account. Checking the SID is how the first
/// version of this list passed against a mutant that had left the grant in
/// place: the string was "AU" and the test looked for "S-1-5-11".
// Same shape as dacl_sddl: the Windows ACL tests use it, and a non-Windows
// build does not compile them.
#[cfg(windows)]
#[allow(dead_code)]
const FORBIDDEN_SDDL: &[(&str, &str)] = &[
    ("AU", "Authenticated Users"),
    ("BU", "BUILTIN\\Users"),
    ("WD", "Everyone"),
];
#[cfg(windows)]
fn wide(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::{iter, os::windows::ffi::OsStrExt};
    value
        .as_ref()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

#[cfg(all(windows, test))]
mod windows_acl_tests {
    //! The DACL tests. These are the only tests that exercise the Windows
    //! security path at all -- the Unix tests are compiled out here, so
    //! without this module the ACL code would ship with zero coverage on the
    //! platform it exists for.
    use super::{dacl_sddl, restrict_to_owner, wide, FORBIDDEN_SDDL, SqliteDocumentStore};
    use box_foundation::BoxPaths;
    use std::path::PathBuf;

    /// Grant Authenticated Users read on a directory, the way a real runtime
    /// directory ends up with it.
    ///
    /// This is the hostile parent the whole change is about: a file created
    /// inside it inherits the grant, and only a protected DACL removes it.
    ///
    /// Authenticated Users is granted *alongside* the running identity, not
    /// instead of it. An earlier version granted only Authenticated Users and
    /// the test could no longer create the file it was about to restrict --
    /// which is the protection working, but it makes the test unable to run.
    /// The real directory has both, so the test does too.
    #[cfg(windows)]
    fn grant_authenticated_users(dir: &std::path::Path) {
        use std::{iter, os::windows::ffi::OsStrExt};
        use windows_sys::Win32::Security::Authorization::{
            GetNamedSecurityInfoW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
            SE_FILE_OBJECT, SetEntriesInAclW, TRUSTEE_IS_NAME, TRUSTEE_IS_UNKNOWN,
            TRUSTEE_W,
        };
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        };

        let mut trustees: Vec<TRUSTEE_W> = Vec::new();
        let mut names: Vec<Vec<u16>> = Vec::new();
        // Everyone a real runtime directory grants: the machine accounts so
        // the daemon can run, plus the interactive user.
        for account in ["Authenticated Users", "SYSTEM", "Administrators"] {
            let os: &std::ffi::OsStr = std::ffi::OsStr::new(account);
            let wide: Vec<u16> = os.encode_wide().chain(iter::once(0)).collect();
            names.push(wide);
        }
        for name in &names {
            trustees.push(TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_NAME,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: name.as_ptr() as *mut u16,
            });
        }

        let access: Vec<EXPLICIT_ACCESS_W> = trustees
            .into_iter()
            .map(|trustee| EXPLICIT_ACCESS_W {
                // GENERIC_READ | GENERIC_EXECUTE, and inherited so a file
                // created inside picks it up.
                grfAccessPermissions: 0x0012_0009,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: 0x3,
                Trustee: trustee,
            })
            .collect();

        // Read the directory existing DACL first and append to it. Passing
        // null for the old DACL here would replace the whole thing with a
        // read-only grant, which locks the runner out of creating the
        // state/ subdirectory -- the protection working, but the test
    // unable to run.
        let wide_dir = wide(dir);
        let mut existing: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
        let read = unsafe {
            GetNamedSecurityInfoW(
                wide_dir.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut existing,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(read, 0, "the existing DACL must be readable");

        let mut acl: *mut windows_sys::Win32::Security::ACL = std::ptr::null_mut();
        let built = unsafe {
            SetEntriesInAclW(
                access.len() as u32,
                access.as_ptr(),
                existing,
                &mut acl,
            )
        };
        assert_eq!(built, 0, "the hostile DACL must build");

        let applied = unsafe {
            SetNamedSecurityInfoW(
                wide_dir.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(applied, 0, "the hostile DACL must be written");
    }
    fn restricted_file(name: &str) -> (PathBuf, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("dshbox-acl-{name}-"))
            .tempdir()
            .unwrap();
        let inner = dir.path().join("state");
        std::fs::create_dir_all(&inner).unwrap();
        // Make the parent hostile first, so the file inherits the grant we
        // are about to claim to remove.
        grant_authenticated_users(dir.path());
        let file = inner.join("secrets.db");
        std::fs::write(&file, b"secret").unwrap();
        restrict_to_owner(&file).expect("the restriction must succeed on a file we own");
        (file, dir)
    }
    #[test]
    fn the_secrets_file_grants_the_three_principals_and_nobody_else() {
        // SYSTEM, the creator owner, and Administrators -- asserted as SDDL
        // SIDs rather than as resolved names, because a name depends on the
        // machine locale and the account the daemon runs as.
        let (file, _dir) = restricted_file("three");
        let sddl = dacl_sddl(&file).expect("the DACL must be readable");
        // S-1-5-18 is SYSTEM and S-1-5-32-544 is BUILTIN\Administrators. The
        // third grant is the running user, and on a file it is resolved to
        // that user domain SID rather than staying the abstract
        // CREATOR OWNER (S-1-3-0), so it is asserted by shape: three grants,
        // the two named ones present, and the third not one of the
        // forbidden accounts.
        // SDDL uses the well-known abbreviations: SY is SYSTEM and BA is
        // BUILTIN\Administrators.
        assert!(sddl.contains("(A;;FA;;;SY)"), "SYSTEM must be granted: {sddl}");
        assert!(sddl.contains("(A;;FA;;;BA)"), "Administrators must be granted: {sddl}");
        let grants = sddl.matches("(A;;FA;;;").count();
        assert_eq!(grants, 3, "exactly three grants, got {grants}: {sddl}");
        // The third grant is the running user, resolved to its domain SID
        // (S-1-5-21-...) rather than staying the abstract CREATOR OWNER.
        let third = sddl
            .match_indices("(A;;FA;;;")
            .map(|(at, _)| &sddl[at + 9..])
            .map(|rest| rest.split(')').next().unwrap_or(""))
            .filter(|sid| *sid != "SY" && *sid != "BA")
            .collect::<Vec<_>>();
        assert_eq!(third.len(), 1, "one user grant, got {third:?}: {sddl}");
        assert!(
            third[0].starts_with("S-1-5-21-"),
            "the third grant must be a domain user SID, got {}: {sddl}",
            third[0]
        );

    }

    #[test]
    fn authenticated_users_is_not_granted() {
        // The whole reason this code exists. A fresh Windows install inherits
        // NT AUTHORITY\Authenticated Users and BUILTIN\Users from the parent
        // directory, which on a multi-user machine is every interactive user
        // holding read on the file with the control-plane private key.
        let (file, _dir) = restricted_file("no-auth-users");
        let sddl = dacl_sddl(&file).expect("the DACL must be readable");
        for (fragment, name) in FORBIDDEN_SDDL {
            // Match the whole ACE, so an abbreviation that happens to appear
            // inside a longer SID is not a false positive.
            let ace = format!(";;;{fragment})");
            assert!(!sddl.contains(&ace), "{name} must not be granted: {sddl}");
        }
    }

    #[test]
    fn a_secrets_store_opens_with_a_restricted_dacl() {
        // The end-to-end case: open the real store, then read back what it
        // wrote. This is the assertion that would have caught PR #51 pointing
        // 0600 at the wrong inode, had it existed then.
        let dir = tempfile::Builder::new()
            .prefix("dshbox-acl-store-")
            .tempdir()
            .unwrap();
        // A hostile parent, exactly as restricted_file builds one. Without
        // this the test passes under the mutant: %TEMP% already grants only
        // SYSTEM, Administrators and the running user, so there is no AU to
        // remove and the inherited DACL satisfies the assertion on its own.
        // A review proved that by running the mutant -- the other two tests
        // went red and this one stayed green, because it was the only one
        // whose environment already met the property it claimed to check.
        grant_authenticated_users(dir.path());
        let paths = BoxPaths {
            config: PathBuf::from("C:\\nonexistent"),
            runtime: Some(dir.path().to_path_buf()),
        };
        let store = SqliteDocumentStore::open_secrets(&paths.secrets_db().unwrap())
            .expect("the secrets store must open");
        drop(store);

        let sddl = dacl_sddl(&paths.secrets_db().unwrap()).expect("the DACL must be readable");
        for (fragment, name) in FORBIDDEN_SDDL {
            // Match the whole ACE, so an abbreviation that happens to appear
            // inside a longer SID is not a false positive.
            let ace = format!(";;;{fragment})");
            assert!(!sddl.contains(&ace), "{name} must not be granted: {sddl}");
        }
    }
}
