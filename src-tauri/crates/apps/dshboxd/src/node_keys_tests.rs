//! Tests for the node-keys store.
//!
//! The properties worth pinning are the four disciplines from design doc
//! section 8, because each one is invisible in the happy path: a secret that
//! leaks to a log line still works, a world-readable database still works,
//! and a stored seed still works -- until the day it does not.

use super::*;
use box_foundation::BoxPaths;
use box_node_link::node_keys::KeyMode;

/// The one row with this id, or None.
///
/// The collection API is load_all plus upsert -- there is no get -- so a
/// single-row read is a scan. That is the store shape, not a shortcut: the
/// same helper is what the daemon will use.
fn find_row(collection: &Collection<NodeKeyRecord>, id: &str) -> Option<NodeKeyRecord> {
    collection
        .load_all()
        .unwrap()
        .into_iter()
        .find(|record| record.id == id)
}

fn paths() -> (BoxPaths, tempfile::TempDir) {
    // A runtime directory of its own, so two tests never share a store. The
    // config path is irrelevant here: only the runtime is read.
    let dir = tempfile::tempdir().unwrap();
    let paths = BoxPaths {
        config: std::path::PathBuf::from("/nonexistent"),
        runtime: Some(dir.path().to_path_buf()),
    };
    (paths, dir)
}

// ---------- disciplines 1 and 3: what is stored ----------

#[test]
fn a_token_record_never_holds_the_seed() {
    // The row carries the chain end, which is H^n(seed). The seed itself is
    // not there, so a stolen row cannot regenerate the chain.
    let record = box_node_link::node_keys::NodeKeyRecord::token("node-1", &"ab".repeat(32), 100);
    assert_eq!(record.mode, KeyMode::Token);
    assert!(record.secret.is_empty(), "the seed must not be stored");
    let chain_end = "ab".repeat(32);
    assert_eq!(record.chain_end.as_deref(), Some(chain_end.as_str()));
}

#[test]
fn the_debug_form_redacts_the_secret() {
    let record = box_node_link::node_keys::NodeKeyRecord::psk("node-1", "super-secret", 1);
    let text = format!("{record:?}");
    assert!(!text.contains("super-secret"), "the secret reached a debug line");
    assert!(text.contains("<redacted>"));
}

#[test]
fn the_control_identity_debug_form_redacts_the_private_key() {
    let identity = ControlIdentity {
        id: "default".to_owned(),
        pubkey: "aa".repeat(32),
        seckey: "bb".repeat(48),
        fingerprint: "0123456789abcdef".to_owned(),
        created_at: 1,
    };
    let text = format!("{identity:?}");
    assert!(!text.contains(&"bb".repeat(48)), "the private key reached a debug line");
    assert!(text.contains("0123456789abcdef"), "the fingerprint should be visible");
}

#[test]
fn the_summary_never_carries_the_secret() {
    let record = box_node_link::node_keys::NodeKeyRecord::psk("node-1", "super-secret", 1);
    let json = serde_json::to_string(&summary_of(&record)).unwrap();
    assert!(!json.contains("super-secret"), "the secret reached an RPC payload");
    assert!(!json.contains("secret"), "the summary has a secret field at all");
}

// ---------- discipline 2: the file on disk ----------

#[test]
fn every_store_file_is_owner_only_while_the_connection_is_open() {
    // The main .db is a near-empty header under WAL -- the bytes are in the
    // -wal sidecar, which is where a review found the PSK and the
    // control-plane private key. So this checks all three files, while the
    // connection is still open.
    //
    // Checking after dropping the collection does not work: SQLite
    // checkpoints and *deletes* the -wal on the last connection close, so the
    // file holding the secret is gone by the time the assertion would run.
    // A first version did that, and passed against a mutant that tightened
    // only the .db.
    //
    // PLATFORM -- read this before trusting a green run anywhere else.
    //
    // The body is cfg(unix), so on Windows this is a no-op: there are no Unix
    // mode bits, and the store files inherit the runtime directory ACL. A
    // review read that ACL back on a fresh Windows install:
    //
    //   NT AUTHORITY\Authenticated Users : Modify, Synchronize
    //   BUILTIN\Users                     : ReadAndExecute, Synchronize
    //
    // both inherited from the parent directory. So on the platform this ships
    // on, every local user can read the file holding the control-plane
    // Ed25519 private key and every node PSK. The 0600 discipline design
    // section 8.2 asks for is NOT IN FORCE on Windows -- not untested, absent.
    //
    // The same cfg(unix) pair in box-resources/transfer/tree.rs is a
    // consistency point, not a sufficiency argument: that guards credential
    // payloads an operator moves deliberately, and this guards the daemon own
    // long-term identity written on every enrol.
    //
    // The fix is tracked as a follow-up, not as this comment:
    // SetNamedSecurityInfo on .db, -wal and -shm, written by a daemon that
    // must not be able to lock itself out. Until it lands, a green run here
    // says the Unix path works.
    let (paths, _dir) = paths();
    let collection = open_node_keys(&paths).unwrap();
    collection
        .upsert(&[box_node_link::node_keys::NodeKeyRecord::psk(
            "node-1", "a-secret", 1,
        )])
        .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let db = paths.store_db().unwrap();
        let mut checked = 0;
        for suffix in ["", "-wal", "-shm"] {
            let candidate = if suffix.is_empty() {
                db.clone()
            } else {
                std::path::PathBuf::from(format!("{}{suffix}", db.display()))
            };
            if !candidate.exists() {
                continue;
            }
            let mode = std::fs::metadata(&candidate).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "{} must be owner-only, got {mode:o}",
                candidate.display()
            );
            checked += 1;
        }
        assert!(
            checked >= 2,
            "expected the db and its wal sidecar while open, found {checked}"
        );
    }
}
#[test]
fn a_loose_store_is_repaired_rather_than_inherited() {
    // Same platform note as the test above: unix-only, and the discipline is
    // absent on Windows. This one closes and reopens, which checkpoints the
    // -wal away, so it covers the main file only.
    // A store left world-readable by an earlier build must be fixed on the
    // next open, not inherited.
    let (paths, _dir) = paths();
    {
        let _ = open_node_keys(&paths).unwrap();
    }
    let db = paths.store_db().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let candidate = if suffix.is_empty() {
                db.clone()
            } else {
                std::path::PathBuf::from(format!("{}{suffix}", db.display()))
            };
            if !candidate.exists() {
                continue;
            }
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let _ = open_node_keys(&paths).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let candidate = if suffix.is_empty() {
                db.clone()
            } else {
                std::path::PathBuf::from(format!("{}{suffix}", db.display()))
            };
            if !candidate.exists() {
                continue;
            }
            assert_eq!(
                std::fs::metadata(&candidate).unwrap().permissions().mode() & 0o777,
                0o600,
                "a loose {} must be repaired on open",
                candidate.display()
            );
        }
    }
}

// ---------- discipline 4: issuing ----------

#[test]
fn a_stored_record_round_trips_and_keeps_its_mode() {
    let (paths, _dir) = paths();
    let collection = open_node_keys(&paths).unwrap();
    let chain_end = "cd".repeat(32);
    let record = box_node_link::node_keys::NodeKeyRecord::token("node-1", &chain_end, 5);
    collection.upsert(&[record.clone()]).unwrap();
    let back = find_row(&collection, "node-1").expect("the row is stored");
    assert_eq!(back, record);
    assert_eq!(mode_of(&back), KeyMode::Token);
}

#[test]
fn a_node_with_no_row_has_no_registered_key() {
    let (paths, _dir) = paths();
    let collection = open_node_keys(&paths).unwrap();
    assert_eq!(registered_pubkey_for(&collection, "never-enrolled").unwrap(), None);
}

#[test]
fn a_revoked_row_is_not_live_and_an_expired_one_is_not_either() {
    let live = box_node_link::node_keys::NodeKeyRecord::psk("n", "s", 0);
    assert!(is_node_live(&live));

    let mut revoked = live.clone();
    revoked.revoked_at = Some(1);
    assert!(!is_node_live(&revoked), "a revoked row must be refused");

    let mut expired = live.clone();
    expired.expires_at = Some(1);
    assert!(!is_node_live(&expired), "an expired row must be refused");
}

// ---------- the control plane identity ----------

#[test]
fn the_control_identity_is_generated_once_and_stable_after() {
    let (paths, _dir) = paths();
    let collection = open_control_identity(&paths).unwrap();
    let first = ensure_control_identity(&collection).unwrap();
    let second = ensure_control_identity(&collection).unwrap();
    assert_eq!(first, second, "the identity must not be regenerated");
    let row = find_identity(&collection).unwrap().unwrap();
    assert!(!row.seckey.is_empty());
    assert_ne!(row.fingerprint, row.seckey);
}

// ---------- node id discipline ----------

#[test]
fn a_node_id_with_a_path_separator_is_refused() {
    // The document id is one TEXT column, and a node id travels through URLs
    // and JSON. Refuse at the boundary rather than escape later.
    assert!(is_node_id("node-1"));
    assert!(is_node_id("node_1.win"));
    assert!(!is_node_id(""));
    assert!(!is_node_id("../etc/passwd"));
    assert!(!is_node_id("a/b"));
    assert!(!is_node_id("a\\b"));
    assert!(!is_node_id("a b"));
    assert!(!is_node_id(&"x".repeat(129)));
}
