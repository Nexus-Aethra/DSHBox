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
use std::io::Write;
use std::path::{Path, PathBuf};

// Wire + on-disk index format, shared through box-api.
pub(crate) use box_api::{DataEntry, DataUse};

/// Download a remote tarball URL and extract it into `destination`.
#[allow(
    dead_code,
    reason = "remote data import is not exposed by the current IPC"
)]
pub(crate) fn download_remote_tarball(url: &str, destination: &Path) -> Result<(), String> {
    let response =
        reqwest::blocking::get(url).map_err(|error| format!("download {url}: {error}"))?;
    let bytes = response
        .bytes()
        .map_err(|error| format!("download {url}: {error}"))?;
    let archive_path = destination
        .parent()
        .unwrap_or(destination)
        .join("archive.bin");
    let mut file = std::fs::File::create(&archive_path)
        .map_err(|error| format!("cannot create archive staging: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("cannot write archive: {error}"))?;
    std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    extract_extension_tarball(&archive_path, destination)?;
    let _ = std::fs::remove_file(&archive_path);
    Ok(())
}

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

#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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
#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
fn content_digest(directory: &Path) -> Result<String, String> {
    box_extensions::extension_digest(directory)
        .map(|value| value.trim_start_matches("fnv1a64:").to_owned())
}

/// Human-readable name for a data source: the store key and the
/// per-container directory name.
#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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
        ParsedSource::GitPrefix { .. }
        | ParsedSource::NpmPrefix { .. }
        | ParsedSource::Passthrough { .. } => {
            // Unreachable in practice — `stage_source` rejects these
            // variants for `ADD data` — but the match must be exhaustive.
            "data".to_owned()
        }
    }
}

#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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
        ParsedSource::GitPrefix { ref_ } => format!("git:{ref_}"),
        ParsedSource::NpmPrefix { spec } => format!("npm:{spec}"),
        ParsedSource::Passthrough { spec } => spec.clone(),
    }
}

/// Entry point for one `ADD data` op: import the payload into the store
/// (or look it up by name) and copy it into the container under
/// `extensions/data/<name>`.
#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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

#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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
#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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
#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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
    let staging = data_root(runtime).join("staging").join(format!(
        "data-{}-{}",
        std::process::id(),
        now_seconds()
    ));
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
#[allow(
    dead_code,
    reason = "only used by the deferred data materialization path"
)]
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
        ParsedSource::GitPrefix { ref_ } => Err(format!(
            "git: prefix is not supported on `ADD data` (use `ADD data {ref_}` without the prefix)"
        )),
        ParsedSource::NpmPrefix { spec } => Err(format!(
            "npm: prefix is only supported on `ADD plugin` (got `ADD data npm:{spec}`)"
        )),
        ParsedSource::Passthrough { spec } => Err(format!(
            "advanced spec `{spec}` is not supported on `ADD data`; use one of: GitHub short-form / local path / local tarball / remote tarball"
        )),
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
    for container in box_containers::scan_containers(&runtime.to_string_lossy())?.into_values() {
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
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) && !in_use.contains(&name) {
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
#[path = "data_tests.rs"]
mod tests;
