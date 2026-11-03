//! Directory-tree copy, in the two semantics the project actually needs.
//!
//! Four private implementations of "copy a directory tree" existed across
//! the daemon, the desktop shell and box-resources — with subtly different
//! symlink behaviour, and one of them (the desktop's) lacked the cycle
//! guard below, so a pnpm junction pointing back at an ancestor would have
//! stacked it. They are consolidated here. box-resources' `copy_tree` is
//! deliberately NOT part of this consolidation: it enforces container-data
//! safety (a symlink escaping the container root is refused, links inside
//! it are preserved) and is a different contract, not a worse copy of this
//! one.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Copy `source` into `destination` with `cp -rL` semantics: symlinks and
/// Windows junctions are resolved and their targets materialised as plain
/// files/directories, never re-exported as links. A link back to an
/// ancestor of the walk is omitted rather than followed — package-manager
/// trees are graphs, and Node resolves that dependency from the already
/// materialised ancestor directory. The destination is created if absent
/// and is otherwise merged into.
pub fn copy_tree_following(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut ancestors = HashSet::new();
    copy_tree_following_inner(source, destination, &mut ancestors)
}

fn copy_tree_following_inner(
    source: &Path,
    destination: &Path,
    ancestors: &mut HashSet<PathBuf>,
) -> std::io::Result<()> {
    // Package-manager trees are graphs, not necessarily trees: pnpm can
    // create a package-local junction that ultimately points back to an
    // ancestor package. `cp -rL` semantics must reject that impossible-to-
    // materialize cycle instead of recursing until the caller stack-overflows.
    let identity = fs::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
    if !ancestors.insert(identity.clone()) {
        // The source is a package-manager graph. A reparse point back to an
        // ancestor cannot be represented by a finite physical tree; omitting
        // this back-edge is safe because Node resolves that dependency from
        // the already materialized ancestor `node_modules` directory.
        return Ok(());
    }
    let result = (|| {
        fs::create_dir_all(destination)
            .map_err(|error| copy_tree_error(error, "create directory", source, destination))?;
        for entry in fs::read_dir(source)
            .map_err(|error| copy_tree_error(error, "read directory", source, destination))?
        {
            let entry = entry.map_err(|error| {
                copy_tree_error(error, "read directory entry", source, destination)
            })?;
            let from = entry.path();
            let to = destination.join(entry.file_name());
            // `symlink_metadata` does not follow symlinks, so a symlink to a
            // directory here still reports `is_symlink()` plus `is_dir()`. We
            // recurse only on plain directories and treat everything else as
            // a leaf whose target we materialise.
            let metadata = fs::symlink_metadata(&from)
                .map_err(|error| copy_tree_error(error, "read metadata", &from, &to))?;
            if metadata.file_type().is_symlink() {
                // Resolve the link, then copy whatever it points at. If the
                // target is a directory we recurse; otherwise we hit the file
                // branch below.
                let resolved = fs::canonicalize(&from).unwrap_or_else(|_| from.clone());
                // `canonicalize` resolves a Windows pnpm junction, but
                // `symlink_metadata` on the resolved path can still report a
                // reparse-point file type. Use `metadata` here deliberately: we
                // need the target's real directory/file kind before deciding
                // between recursive copy and `fs::copy`.
                let resolved_meta = fs::metadata(&resolved).map_err(|error| {
                    copy_tree_error(error, "read dereferenced metadata", &resolved, &to)
                })?;
                if resolved_meta.is_dir() {
                    copy_tree_following_inner(&resolved, &to, ancestors).map_err(|error| {
                        copy_tree_error(error, "copy dereferenced directory", &resolved, &to)
                    })?;
                } else {
                    fs::copy(&resolved, &to).map_err(|error| {
                        copy_tree_error(error, "copy dereferenced file", &resolved, &to)
                    })?;
                }
            } else if metadata.is_dir() {
                copy_tree_following_inner(&from, &to, ancestors)
                    .map_err(|error| copy_tree_error(error, "copy directory", &from, &to))?;
            } else if metadata.is_file() {
                fs::copy(&from, &to)
                    .map_err(|error| copy_tree_error(error, "copy file", &from, &to))?;
            }
        }
        Ok(())
    })();
    ancestors.remove(&identity);
    result
}

fn copy_tree_error(
    error: std::io::Error,
    action: &str,
    source: &Path,
    destination: &Path,
) -> std::io::Error {
    std::io::Error::new(
        error.kind(),
        format!(
            "{action} {} -> {}: {error}",
            source.display(),
            destination.display()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sandbox(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-copytree-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copies_plain_tree_recursively() {
        let src = sandbox("plain");
        let dst = sandbox("plain-dst");
        fs::write(src.join("hello.txt"), "hi").unwrap();
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested/inner.txt"), "deep").unwrap();

        copy_tree_following(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("hello.txt")).unwrap(), "hi");
        assert!(dst.join("nested/inner.txt").is_file());
        assert_eq!(
            fs::read_to_string(dst.join("nested/inner.txt")).unwrap(),
            "deep"
        );
    }

    #[test]
    #[cfg(unix)]
    fn dereferences_internal_symlinks() {
        use std::os::unix::fs::symlink;
        let src = sandbox("sym");
        let dst = sandbox("sym-dst");
        fs::write(src.join("target.txt"), "content").unwrap();
        symlink("target.txt", src.join("link")).unwrap();

        copy_tree_following(&src, &dst).unwrap();
        // The resulting dir must contain a plain file named `link` whose
        // contents match the symlink target — not another symlink.
        let metadata = fs::symlink_metadata(dst.join("link")).unwrap();
        assert!(!metadata.file_type().is_symlink(), "expected plain file");
        assert_eq!(fs::read_to_string(dst.join("link")).unwrap(), "content");
    }

    #[test]
    #[cfg(unix)]
    fn terminates_when_a_link_points_to_an_ancestor() {
        use std::os::unix::fs::symlink;
        let src = sandbox("cycle");
        let dst = sandbox("cycle-dst");
        fs::write(src.join("real.txt"), "content").unwrap();
        symlink(".", src.join("cycle")).unwrap();

        copy_tree_following(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("real.txt")).unwrap(), "content");
        assert!(!dst.join("cycle").exists());
    }

    #[test]
    fn does_not_error_on_pre_existing_unrelated_destination() {
        let src = sandbox("noop");
        let dst = sandbox("noop-dst");
        fs::write(src.join("x.txt"), "x").unwrap();
        // `create_dir_all` on a path that already exists is fine — copy
        // must not error out on its own.
        fs::create_dir_all(&dst).unwrap();
        copy_tree_following(&src, &dst).unwrap();
        assert!(dst.join("x.txt").is_file());
    }
}
