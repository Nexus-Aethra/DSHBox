//! What pnpm already has on disk for this runtime directory.
//!
//! Box gives pnpm a private store (`PNPM_CONFIG_STORE_DIR` →
//! `<runtime>/pnpm/store`), so an install that hits it needs no network. The
//! store keeps a content-addressed `files/` tree plus an index whose rows are
//! `<integrity>\t<name>@<version>` — the index keys are all that is needed to
//! answer "is this package cached?", without touching the package contents.
//!
//! The layout is pnpm's own and has changed before (`store/v3/index/*.json`
//! became `store/v11/index.db`), so an unrecognised store is an error: the
//! caller reports "unknown", never "not cached".

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The `name@version` specs pnpm has cached for this runtime directory.
pub fn cached_packages(runtime_dir: &Path) -> Result<BTreeSet<String>, String> {
    let store = runtime_dir.join("pnpm").join("store");
    let index = find_index_db(&store)
        .ok_or_else(|| format!("no pnpm store index under {}", store.display()))?;
    let connection = rusqlite::Connection::open_with_flags(
        &index,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|error| format!("cannot read {}: {error}", index.display()))?;
    let mut statement = connection
        .prepare("SELECT key FROM package_index")
        .map_err(|error| format!("{} is not a pnpm store index: {error}", index.display()))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut packages = BTreeSet::new();
    for row in rows {
        let key = row.map_err(|error| error.to_string())?;
        // `sha512-…\tname@version`; older stores used the same join.
        if let Some(spec) = key.split('\t').next_back() {
            if spec.contains('@') && !spec.is_empty() {
                packages.insert(spec.to_owned());
            }
        }
    }
    Ok(packages)
}

/// `<store>/v<version>/index.db`, whichever version directory pnpm created.
fn find_index_db(store: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(store).ok()?;
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join("index.db"))
        .filter(|path| path.is_file())
        .collect();
    candidates.sort();
    // Highest version wins when several stores coexist after a pnpm upgrade.
    candidates.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(keys: &[&str]) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("box-pnpm-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dir = root.join("pnpm").join("store").join("v11");
        std::fs::create_dir_all(&dir).unwrap();
        let connection = rusqlite::Connection::open(dir.join("index.db")).unwrap();
        connection
            .execute("CREATE TABLE package_index (key TEXT PRIMARY KEY, data BLOB)", [])
            .unwrap();
        for key in keys {
            connection
                .execute("INSERT INTO package_index (key, data) VALUES (?1, x'')", [key])
                .unwrap();
        }
        (root.clone(), root)
    }

    #[test]
    fn reads_the_cached_specs_out_of_the_index() {
        let (runtime, _guard) = store_with(&[
            "sha512-aaa\t@nexus-aethra/dshell-ssh@0.1.5",
            "sha512-bbb\tzod@4.4.3",
        ]);
        let cached = cached_packages(&runtime).unwrap();
        assert!(cached.contains("@nexus-aethra/dshell-ssh@0.1.5"));
        assert!(cached.contains("zod@4.4.3"));
        assert_eq!(cached.len(), 2);
    }

    #[test]
    fn an_unknown_store_is_an_error_not_an_empty_answer() {
        let root = std::env::temp_dir().join(format!("box-pnpm-store-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(cached_packages(&root).is_err());
    }
}
