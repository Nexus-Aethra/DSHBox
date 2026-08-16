//! Content-addressed data store for `ADD data` payloads.
//!
//! Data payloads live at `<root>/data/<digest>/`, deduplicated by content
//! digest (fnv1a64 hex, the same algorithm image-manifest digests use), and
//! are materialised into each container as an independent copy under
//! `extensions/data/<name>`. Unlike repository extensions, data carries no
//! reference counting: the per-container copy lives and dies with its
//! container, and orphaned store entries are garbage-collected by
//! `dshbox image prune`, which scans actual container usage.

use box_containers::DshContainer;
use box_extensions::transfer::{copy_extension_source, extract_extension_tarball};
use box_foundation::{mirror_url, now_seconds, read_config};
use box_image::ParsedSource;
use box_runtime::shallow_clone_with_cancel;
use box_scheduler::TaskContext;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// Wire + on-disk index format, shared through box-api.
pub(crate) use box_api::{DataEntry, DataUse};

use crate::image::download_remote_tarball;

pub(crate) fn data_root(runtime: &Path) -> PathBuf {
    runtime.join("data")
}

fn data_index_path(runtime: &Path) -> PathBuf {
    data_root(runtime).join("index.json")
}

fn read_data_index(runtime: &Path) -> BTreeMap<String, DataEntry> {
    fs::read_to_string(data_index_path(runtime))
        .ok()
        .and_then(|source| serde_json::from_str(&source).ok())
        .unwrap_or_default()
}

fn write_data_index(runtime: &Path, index: &BTreeMap<String, DataEntry>) -> Result<(), String> {
    let path = data_index_path(runtime);
    fs::create_dir_all(path.parent().ok_or("data index has no parent")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        &path,
        serde_json::to_string_pretty(index).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn container_data_path(container: &DshContainer) -> PathBuf {
    PathBuf::from(&container.directory).join("state/data.json")
}

fn read_container_data(container: &DshContainer) -> Vec<DataUse> {
    fs::read_to_string(container_data_path(container))
        .ok()
        .and_then(|source| serde_json::from_str(&source).ok())
        .unwrap_or_default()
}

fn write_container_data(container: &DshContainer, uses: &[DataUse]) -> Result<(), String> {
    let path = container_data_path(container);
    fs::create_dir_all(path.parent().ok_or("container data record has no parent")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        &path,
        serde_json::to_string_pretty(uses).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

/// fnv1a64 hex digest of a directory tree. Reuses the extension digest
/// walker but strips its `fnv1a64:` prefix so the value matches the pure
/// hex digests used by image manifests.
fn content_digest(directory: &Path) -> Result<String, String> {
    box_extensions::extension_digest(directory)
        .map(|value| value.trim_start_matches("fnv1a64:").to_owned())
}

/// Human-readable name for a data source: the store key and the
/// per-container directory name.
fn data_source_name(source: &ParsedSource) -> String {
    match source {
        ParsedSource::Github { url, .. } => url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("data")
            .trim_end_matches(".git")
            .to_owned(),
        ParsedSource::Tarball { url, .. } => {
            let stem = url
                .split('?')
                .next()
                .unwrap_or(url)
                .rsplit('/')
                .next()
                .unwrap_or("data")
                .to_owned();
            let lower = stem.to_ascii_lowercase();
            for suffix in [".tar.gz", ".tar.xz", ".tgz", ".txz", ".tar"] {
                if lower.ends_with(suffix) {
                    return stem[..stem.len() - suffix.len()].to_owned();
                }
            }
            stem
        }
        ParsedSource::LocalDir { path } => path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("data")
            .to_owned(),
        ParsedSource::BareName { name, .. } => name.clone(),
    }
}

fn describe_source(source: &ParsedSource) -> String {
    match source {
        ParsedSource::Github { url, ref_ } => match ref_ {
            Some(reference) => format!("{url}@{reference}"),
            None => url.clone(),
        },
        ParsedSource::Tarball { url, .. } => url.clone(),
        ParsedSource::LocalDir { path } => path.to_string_lossy().into_owned(),
        ParsedSource::BareName {
            name,
            scope,
            version,
        } => {
            let head = match scope {
                Some(scope) => format!("@{scope}/{name}"),
                None => name.clone(),
            };
            match version {
                Some(version) => format!("{head}@{version}"),
                None => head,
            }
        }
    }
}

/// Entry point for one `ADD data` op: import the payload into the store
/// (or look it up by name) and copy it into the container under
/// `extensions/data/<name>`.
pub(crate) fn materialize_data_add(
    task: &TaskContext,
    container: &DshContainer,
    source: &ParsedSource,
) -> Result<(), String> {
    let runtime = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    materialize_data_add_at(Path::new(&runtime), task, container, source)
}

fn materialize_data_add_at(
    runtime: &Path,
    task: &TaskContext,
    container: &DshContainer,
    source: &ParsedSource,
) -> Result<(), String> {
    let entry = import_or_resolve(task, runtime, source)?;
    let store_dir = data_root(runtime).join(&entry.digest);
    if !store_dir.is_dir() {
        return Err(format!(
            "data store entry is missing: {} ({})",
            entry.name, entry.digest
        ));
    }
    let destination = PathBuf::from(&container.directory)
        .join("extensions")
        .join("data")
        .join(&entry.name);
    task.log(&format!(
        "materialising data {} -> extensions/data/{}",
        entry.digest, entry.name
    ));
    copy_extension_source(&store_dir, &destination)?;
    let mut uses = read_container_data(container);
    uses.retain(|item| item.name != entry.name);
    uses.push(DataUse {
        name: entry.name.clone(),
        digest: entry.digest.clone(),
    });
    write_container_data(container, &uses)?;
    Ok(())
}

/// Hard-copy one stored snapshot into a container (image-driven creation).
/// `destination` is container-relative (e.g. `profile/skills/foo` or
/// `extensions/data/foo`). The copy is fully detached from the store —
/// in-container edits never write back. Data-kind snapshots keep the usual
/// `state/data.json` bookkeeping so `image prune` knows real usage.
pub(crate) fn hard_copy_snapshot(
    runtime: &Path,
    container: &DshContainer,
    kind: &str,
    name: &str,
    digest: &str,
    destination: &str,
) -> Result<(), String> {
    let store_dir = data_root(runtime).join(digest);
    if !store_dir.is_dir() {
        return Err(format!(
            "snapshot `{name}` (digest {digest}) is missing from the data store; rebuild the image"
        ));
    }
    let target = PathBuf::from(&container.directory).join(destination.trim_start_matches('/'));
    copy_extension_source(&store_dir, &target)?;
    if kind == "data" {
        let mut uses = read_container_data(container);
        uses.retain(|item| item.name != name);
        uses.push(DataUse {
            name: name.to_owned(),
            digest: digest.to_owned(),
        });
        write_container_data(container, &uses)?;
    }
    Ok(())
}

/// Import a data source into the store, or resolve a bare name against the
/// store index. Returns the store entry.
///
/// Also the snapshot primitive of the image build pipeline: every
/// non-plugin ADD is staged through here so its content lands in
/// `data/<digest>/` with a `name -> digest` mapping (spec:
/// docs/specs/image-build.md).
pub(crate) fn import_or_resolve(
    task: &TaskContext,
    runtime: &Path,
    source: &ParsedSource,
) -> Result<DataEntry, String> {
    if let ParsedSource::BareName { name, .. } = source {
        let index = read_data_index(runtime);
        return index.get(name).cloned().ok_or_else(|| {
            format!(
                "data `{name}` is not in the data store; import it first with `ADD data <source>` (local directory, tarball, or GitHub URL)"
            )
        });
    }
    let name = data_source_name(source);
    let staging = data_root(runtime)
        .join("staging")
        .join(format!("data-{}-{}", std::process::id(), now_seconds()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let source_dir = stage_source(task, source, &staging)?;
    let digest = content_digest(&source_dir)?;
    let store_dir = data_root(runtime).join(&digest);
    if !store_dir.is_dir() {
        fs::create_dir_all(data_root(runtime)).map_err(|error| error.to_string())?;
        fs::rename(&source_dir, &store_dir).map_err(|error| error.to_string())?;
    }
    let _ = fs::remove_dir_all(&staging);
    let entry = DataEntry {
        name: name.clone(),
        digest,
        imported_at: now_seconds(),
        source: describe_source(source),
    };
    let mut index = read_data_index(runtime);
    index.insert(name, entry.clone());
    write_data_index(runtime, &index)?;
    Ok(entry)
}

/// Stage one non-bare data source into `staging/source` and return the
/// content root (the directory tree whose digest identifies the payload).
fn stage_source(
    task: &TaskContext,
    source: &ParsedSource,
    staging: &Path,
) -> Result<PathBuf, String> {
    match source {
        ParsedSource::Github { url, ref_ } => {
            let config = read_config()?;
            let target = mirror_url(url, config.github_mirror.as_deref());
            let destination = staging.join("source");
            task.log(&format!("cloning GitHub repository {url}"));
            let cancelled = task.clone();
            shallow_clone_with_cancel(&target, &destination, ref_.as_deref(), move || {
                cancelled.cancelled()
            })?;
            Ok(destination)
        }
        ParsedSource::Tarball { url, local } => {
            let destination = staging.join("source");
            if *local {
                let archive = PathBuf::from(url);
                if !archive.is_file() {
                    return Err(format!("tarball `{url}` does not exist"));
                }
                task.log(&format!("extracting local tarball {}", archive.display()));
                extract_extension_tarball(&archive, &destination)?;
            } else {
                task.log(&format!("downloading tarball {url}"));
                download_remote_tarball(url, &destination)?;
            }
            Ok(destination)
        }
        ParsedSource::LocalDir { path } => {
            task.log(&format!("copying local directory {}", path.display()));
            let destination = staging.join("source");
            copy_extension_source(path, &destination)?;
            Ok(destination)
        }
        ParsedSource::BareName { .. } => {
            Err("bare data names are resolved from the store index".to_owned())
        }
    }
}

/// Lists the data-store index entries for the UI's image tab.
pub(crate) fn list_data_entries() -> Result<Vec<DataEntry>, String> {
    let runtime = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(read_data_index(Path::new(&runtime)).into_values().collect())
}

/// Garbage-collect the data store: remove every `<digest>` directory not
/// referenced by any container's `state/data.json`, then drop index entries
/// whose store directory is gone. Returns the removed digests.
pub(crate) fn prune_orphaned_data() -> Result<Vec<String>, String> {
    let runtime = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    prune_orphaned_data_at(Path::new(&runtime))
}

fn prune_orphaned_data_at(runtime: &Path) -> Result<Vec<String>, String> {
    let store = data_root(runtime);
    if !store.is_dir() {
        return Ok(Vec::new());
    }
    let mut in_use: HashSet<String> = HashSet::new();
    for container in box_containers::scan_containers(&runtime.to_string_lossy())?
        .into_values()
    {
        for item in read_container_data(&container) {
            in_use.insert(item.digest);
        }
    }
    let mut removed = Vec::new();
    for entry in fs::read_dir(&store).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "index.json" || name == "staging" {
            continue;
        }
        if entry
            .file_type()
            .map(|kind| kind.is_dir())
            .unwrap_or(false)
            && !in_use.contains(&name)
        {
            fs::remove_dir_all(entry.path()).map_err(|error| error.to_string())?;
            removed.push(name);
        }
    }
    let mut index = read_data_index(runtime);
    index.retain(|_, entry| data_root(runtime).join(&entry.digest).is_dir());
    write_data_index(runtime, &index)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use std::env;

    struct NoopNotifier;
    impl box_scheduler::TaskNotifier for NoopNotifier {
        fn stage(&self, _task_id: &str, _stage: &str, _progress: u8) {}
        fn log(&self, _task_id: &str, _line: &str) {}
    }

    fn test_root(name: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("dshboxd-data-{name}-{}", now_seconds()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_task(runtime: &Path) -> TaskContext {
        TaskContext {
            manager: TaskManager::default(),
            paths: BoxPaths {
                config: runtime.join("config.json"),
                runtime: Some(runtime.to_path_buf()),
            },
            notifier: std::sync::Arc::new(NoopNotifier),
            task_id: "test".to_owned(),
        }
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(root).unwrap();
        for (name, content) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    fn local_dir_source(path: &Path) -> ParsedSource {
        ParsedSource::LocalDir {
            path: path.to_path_buf(),
        }
    }

    fn bare_name_source(name: &str) -> ParsedSource {
        ParsedSource::BareName {
            name: name.to_owned(),
            scope: None,
            version: None,
        }
    }

    fn test_container(runtime: &Path, id: &str) -> DshContainer {
        let directory = runtime.join("instances").join(id);
        fs::create_dir_all(&directory).unwrap();
        let container = DshContainer {
            id: id.to_owned(),
            name: id.to_owned(),
            version: "test".to_owned(),
            profile: "web".to_owned(),
            template: None,
            directory: directory.to_string_lossy().into_owned(),
            status: "stopped".to_owned(),
        };
        fs::write(
            directory.join("container.json"),
            serde_json::to_string_pretty(&container).unwrap(),
        )
        .unwrap();
        container
    }

    #[test]
    fn data_source_name_derives_a_sensible_store_key() {
        assert_eq!(
            data_source_name(&ParsedSource::Github {
                url: "https://github.com/team/media-pack".to_owned(),
                ref_: None,
            }),
            "media-pack"
        );
        assert_eq!(
            data_source_name(&ParsedSource::Tarball {
                url: "https://intranet/datasets.tgz".to_owned(),
                local: false,
            }),
            "datasets"
        );
        assert_eq!(
            data_source_name(&ParsedSource::Tarball {
                url: "./packs/audio.tar.gz".to_owned(),
                local: true,
            }),
            "audio"
        );
        assert_eq!(
            data_source_name(&ParsedSource::LocalDir {
                path: PathBuf::from("/work/datasets-v2"),
            }),
            "datasets-v2"
        );
        assert_eq!(
            data_source_name(&bare_name_source("datasets-v2")),
            "datasets-v2"
        );
    }

    #[test]
    fn import_stores_by_digest_and_deduplicates_identical_content() {
        let runtime = test_root("dedupe");
        let task = test_task(&runtime);
        let first_source = runtime.join("sources/first");
        write_tree(&first_source, &[("README.md", "hello"), ("data.bin", "\x00\x01")]);

        let first = import_or_resolve(&task, &runtime, &local_dir_source(&first_source)).unwrap();
        assert_eq!(first.name, "first");
        assert_eq!(first.digest.len(), 16);
        assert!(data_root(&runtime).join(&first.digest).is_dir());

        // Identical content (different source name) deduplicates to the same
        // digest and reuses the existing store directory.
        let second_source = runtime.join("sources/second");
        write_tree(&second_source, &[("README.md", "hello"), ("data.bin", "\x00\x01")]);
        let second = import_or_resolve(&task, &runtime, &local_dir_source(&second_source)).unwrap();
        assert_eq!(second.digest, first.digest);
        assert_ne!(second.name, first.name);

        let index = read_data_index(&runtime);
        assert_eq!(index.len(), 2);
        assert_eq!(index["first"].digest, first.digest);
        assert_eq!(index["second"].digest, first.digest);
        let staging = data_root(&runtime).join("staging");
        assert_eq!(fs::read_dir(&staging).map(|entries| entries.count()).unwrap_or(0), 0);
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn materialize_copies_into_container_and_records_usage() {
        let runtime = test_root("materialize");
        let task = test_task(&runtime);
        let source = runtime.join("sources/payload");
        write_tree(&source, &[("config.yaml", "k: v"), ("assets/logo.png", "png")]);
        let container = test_container(&runtime, "c1");

        materialize_data_add_at(&runtime, &task, &container, &local_dir_source(&source)).unwrap();

        let payload = PathBuf::from(&container.directory)
            .join("extensions/data/payload");
        assert!(payload.join("config.yaml").is_file());
        assert!(payload.join("assets/logo.png").is_file());
        let uses = read_container_data(&container);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "payload");
        assert_eq!(uses[0].digest.len(), 16);
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn bare_name_resolves_from_index_and_errors_when_unknown() {
        let runtime = test_root("bare");
        let task = test_task(&runtime);
        let missing = import_or_resolve(&task, &runtime, &bare_name_source("ghost"));
        assert!(missing.is_err());
        assert!(missing.unwrap_err().contains("not in the data store"));

        let source = runtime.join("sources/payload");
        write_tree(&source, &[("a.txt", "a")]);
        import_or_resolve(&task, &runtime, &local_dir_source(&source)).unwrap();
        let resolved = import_or_resolve(&task, &runtime, &bare_name_source("payload")).unwrap();
        assert_eq!(resolved.name, "payload");
        assert_eq!(resolved.digest.len(), 16);
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn prune_removes_orphans_but_keeps_digests_in_use() {
        let runtime = test_root("prune");
        let task = test_task(&runtime);
        let kept_source = runtime.join("sources/kept");
        write_tree(&kept_source, &[("k.txt", "keep me")]);
        let orphan_source = runtime.join("sources/orphan");
        write_tree(&orphan_source, &[("o.txt", "orphan me")]);

        let kept = import_or_resolve(&task, &runtime, &local_dir_source(&kept_source)).unwrap();
        let orphan = import_or_resolve(&task, &runtime, &local_dir_source(&orphan_source)).unwrap();
        let container = test_container(&runtime, "c1");
        materialize_data_add_at(&runtime, &task, &container, &local_dir_source(&kept_source)).unwrap();

        let removed = prune_orphaned_data_at(&runtime).unwrap();
        assert!(removed.contains(&orphan.digest));
        assert!(!removed.contains(&kept.digest));
        assert!(data_root(&runtime).join(&kept.digest).is_dir());
        assert!(!data_root(&runtime).join(&orphan.digest).exists());

        // The index drops entries whose store directory was removed.
        let index = read_data_index(&runtime);
        assert!(index.contains_key("kept"));
        assert!(!index.contains_key("orphan"));

        // A second prune is a no-op (no more orphans).
        assert!(prune_orphaned_data_at(&runtime).unwrap().is_empty());
        let _ = fs::remove_dir_all(&runtime);
    }
}