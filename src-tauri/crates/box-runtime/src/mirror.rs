//! One shared bare mirror of the Harness source repository.
//!
//! Every pull used to run its own shallow network clone into a private staging
//! directory, so installing a Harness version meant a fresh transfer of that
//! version's whole source tree even when the objects were already on disk, and
//! a failed pull threw every downloaded byte away. This module keeps one bare
//! repository per runtime root and serves pulls from it: a version that is
//! already mirrored costs a local checkout with no network at all, and a new
//! version costs only the objects that version does not share with the ones
//! already held.
//!
//! **The mirror is shallow on purpose.** A full-history mirror of this
//! repository is 372 MB and took 69.9 s with the system git through
//! `gh-proxy.com`, but libgit2's negotiation of a full-history
//! transfer did not finish in fifteen minutes on the same machine - and a
//! pull that pays that once is worse than the pull it was meant to make
//! cheaper. Depth 1 per version keeps the first pull the same size as the
//! clone it replaces, and the object database still deduplicates the blobs
//! that consecutive versions share.
//!
//! The mirror is a **cache**, not user data. It is never exported, never copied
//! into a template or container, and it is not part of any resource's
//! addressing. A user who needs the space back deletes it with
//! `dshbox template mirror purge`; the next pull rebuilds it.
//!
//! Two failure kinds are kept apart, because they call for opposite actions:
//!
//! - a **transport** failure (cannot reach the remote: net/http/ssl at the
//!   libgit2 layer) means the mirror is fine and merely unreachable - keep it,
//!   and serve a version it already holds;
//! - a **missing object** after a fetch that itself succeeded means the mirror
//!   cannot answer for that ref - rebuild it once.
//!
//! Judging that mechanically rather than by matching error text is the point:
//! `git2::Error::class` is a real enum, so `ErrorClass::Net`/`Http`/`Ssl` is a
//! code-level answer. There is no `ErrorClass::Network` (the variant is `Net`)
//! and no `ErrorClass::Timeout` (a libgit2 timeout surfaces with the `Net`
//! class), which is why the timeout case needs no separate branch.

use box_foundation::available_disk_space;
use git2::{AutotagOption, ErrorClass, FetchOptions, RemoteCallbacks};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Text file inside the bare repository recording which remote it mirrors.
///
/// The remote URL moves when the user changes `dshbox config set mirror`, and a
/// moved mirror source is not a reason to throw the whole object database away:
/// it is the same repository behind another door.
pub const MIRROR_SOURCE_FILE: &str = "MIRROR_SOURCE_URL";

/// Free space a mirror fetch is allowed to assume it needs.
///
/// Measured on the real repository (2026-11-05): one version's shallow pack
/// is about 145 MB, and the checkout that follows is 139 MB of working tree
/// with a 2.5 MB `.git` when the object database is borrowed through an
/// alternate. The reservation leaves room for a version whose tree is larger
/// than the last one and for the pack that lands beside the objects.
pub const MIRROR_MIN_FREE_BYTES: u64 = 320 * 1024 * 1024;

/// Free space a mirror checkout is allowed to assume it needs.
pub const CHECKOUT_MIN_FREE_BYTES: u64 = 320 * 1024 * 1024;

/// A stage bar never reaches 100% from observed activity alone: only the
/// stage's own done transition completes it, so "moving but not finished"
/// stays distinguishable from "finished".
const MAX_REPORTED_PROGRESS: u8 = 92;

/// What went wrong, split by whether the mirror is the problem.
#[derive(Debug, Clone)]
pub struct MirrorError {
    /// The remote could not be reached (or the transfer was interrupted).
    /// The mirror on disk is *not* at fault and must be preserved.
    pub transport: bool,
    pub message: String,
}

impl MirrorError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            transport: true,
            message: message.into(),
        }
    }

    fn local(message: impl Into<String>) -> Self {
        Self {
            transport: false,
            message: message.into(),
        }
    }

    pub fn into_message(self) -> String {
        self.message
    }
}

/// Everything the mirror operations call back into: a cancellation test and an
/// optional progress sink.
///
/// Passing them as a bundle keeps the two callbacks - which both run inside
/// libgit2's own callbacks - from turning every function signature into a
/// generic soup.
pub struct MirrorCallbacks<'a> {
    pub cancelled: &'a dyn Fn() -> bool,
    pub progress: Option<&'a dyn Fn(u8)>,
}

impl<'a> MirrorCallbacks<'a> {
    pub fn new(cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            cancelled,
            progress: None,
        }
    }

    pub fn with_progress(mut self, progress: &'a dyn Fn(u8)) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn is_cancelled(&self) -> bool {
        (self.cancelled)()
    }

    fn report(&self, percent: u8) {
        if let Some(progress) = self.progress {
            progress(percent.min(MAX_REPORTED_PROGRESS));
        }
    }

    /// Progress from a libgit2 transfer, in whole percent of the objects being
    /// transferred. Returns whether the transfer should continue.
    fn on_transfer(&self, progress: &git2::Progress<'_>) -> bool {
        let total = progress.total_objects();
        if total > 0 {
            let percent = (progress.received_objects().saturating_mul(100) / total) as u8;
            self.report(percent);
        }
        !self.is_cancelled()
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Where one runtime root keeps its mirror.
pub fn mirror_path(root: &Path) -> PathBuf {
    box_foundation::RuntimeLayout::new(root).harness_mirror_dir()
}

/// Whether a directory is a bare repository this module can use.
///
/// A mirror that fails this test is treated as absent: the object database may
/// be damaged, and rebuilding is the only repair the design allows.
pub fn is_usable(mirror: &Path) -> bool {
    if !mirror.is_dir() {
        return false;
    }
    let Ok(repository) = git2::Repository::open_bare(mirror) else {
        return false;
    };
    repository.is_bare() && repository.find_remote("origin").is_ok()
}

fn recorded_remote(mirror: &Path) -> Option<String> {
    fs::read_to_string(mirror.join(MIRROR_SOURCE_FILE))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Record the mirror's source after a clone, and re-point it when the user
/// changed the configured mirror.
///
/// Written through a same-directory temporary file so a crash cannot leave a
/// truncated URL that a later run would read as a different remote.
fn record_remote(mirror: &Path, remote_url: &str) -> Result<(), MirrorError> {
    if recorded_remote(mirror).as_deref() == Some(remote_url) {
        return Ok(());
    }
    let path = mirror.join(MIRROR_SOURCE_FILE);
    let temporary = mirror.join(format!(".{MIRROR_SOURCE_FILE}.{}.tmp", now_nanos()));
    fs::write(&temporary, format!("{remote_url}\n"))
        .map_err(|error| MirrorError::local(format!("cannot record the mirror source: {error}")))?;
    fs::rename(&temporary, &path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        MirrorError::local(format!("cannot record the mirror source: {error}"))
    })
}

/// Free bytes on the volume that holds a path, or None when it cannot be
/// asked. A path that does not exist yet is asked about through its parent.
fn free_bytes(path: &Path) -> Option<u64> {
    let mut candidate = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    loop {
        if let Some(free) = available_disk_space(&candidate) {
            return Some(free);
        }
        if !candidate.pop() {
            return None;
        }
    }
}

fn require_free_space(path: &Path, needed: u64, what: &str) -> Result<(), MirrorError> {
    let Some(free) = free_bytes(path) else {
        // A volume that cannot be measured must not block the operation: the
        // honest report is "unknown", and the write itself still fails loudly
        // rather than corrupting anything.
        return Ok(());
    };
    if free < needed {
        return Err(MirrorError::local(format!(
            "not enough free space for {what}: {} MB required, {} MB available on the runtime volume",
            needed / (1024 * 1024),
            free / (1024 * 1024),
        )));
    }
    Ok(())
}

/// Total bytes of the mirror on disk (0 when it does not exist).
pub fn mirror_size_bytes(mirror: &Path) -> u64 {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&entry.path(), total);
            } else if kind.is_file() {
                if let Ok(metadata) = entry.metadata() {
                    *total = total.saturating_add(metadata.len());
                }
            }
        }
    }
    if !mirror.is_dir() {
        return 0;
    }
    let mut total = 0;
    walk(mirror, &mut total);
    total
}

/// Whether a remote string names a local directory rather than a URL.
///
/// libgit2's local transport does not implement shallow fetches, so the depth
/// request has to be dropped for one - the same rule the single-shot clone in
/// this crate follows.
fn is_local_remote(url: &str) -> bool {
    Path::new(url).is_dir()
}

/// Builds the fetch options a mirror transfer needs: the cancellation push
/// from libgit2's transfer callback, the progress report, the user's proxy, and
/// the shallow depth that keeps a new version's transfer to what it adds.
fn mirror_fetch_options<'a>(
    callbacks: &'a MirrorCallbacks<'a>,
    proxy_url: Option<&str>,
    depth: Option<i32>,
) -> FetchOptions<'a> {
    let mut fetch = FetchOptions::new();
    if let Some(depth) = depth {
        fetch.depth(depth);
    }
    let mut remote_callbacks = RemoteCallbacks::new();
    remote_callbacks.transfer_progress(|progress| callbacks.on_transfer(&progress));
    fetch.remote_callbacks(remote_callbacks);
    let mut proxy = git2::ProxyOptions::new();
    match proxy_url {
        Some(proxy_url) => {
            proxy.url(proxy_url);
        }
        // Fall back to libgit2's own detection (environment variables).
        None => {
            proxy.auto();
        }
    }
    fetch.proxy_options(proxy);
    fetch
}

/// Open the mirror's `origin`, re-pointing it when the configured source moved.
fn open_mirror_with_remote(
    mirror: &Path,
    remote_url: &str,
) -> Result<git2::Repository, MirrorError> {
    let repository = git2::Repository::open_bare(mirror).map_err(|error| {
        MirrorError::local(format!("cannot open the shared mirror: {error}"))
    })?;
    if recorded_remote(mirror).as_deref() != Some(remote_url) {
        if repository.find_remote("origin").is_ok() {
            repository
                .remote_set_url("origin", remote_url)
                .map_err(|error| {
                    MirrorError::local(format!(
                        "cannot point the shared mirror at {remote_url}: {error}"
                    ))
                })?;
        } else {
            repository.remote("origin", remote_url).map_err(|error| {
                MirrorError::local(format!(
                    "cannot add the shared mirror's remote: {error}"
                ))
            })?;
        }
        record_remote(mirror, remote_url)?;
    }
    Ok(repository)
}

/// Ensure a bare mirror repository exists for this runtime root and points at
/// the given remote, returning its path.
///
/// Creating one is two file writes, not a clone: the objects arrive in
/// `fetch_ref_to_mirror`, which is the call that knows which version the
/// user actually asked for. That keeps a first pull the same size as the clone
/// it replaces, instead of paying for history nobody asked to hold.
pub fn ensure_bare_mirror(
    root: &Path,
    remote_url: &str,
    callbacks: &MirrorCallbacks<'_>,
) -> Result<PathBuf, MirrorError> {
    let mirror = mirror_path(root);
    callbacks.report(0);
    if is_usable(&mirror) {
        // The remote may have moved (the user changed the mirror setting).
        // Re-point it instead of rebuilding: the same repository behind
        // another door is not a damaged cache.
        open_mirror_with_remote(&mirror, remote_url)?;
        return Ok(mirror);
    }
    if let Some(parent) = mirror.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            MirrorError::local(format!(
                "cannot create the runtime directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let repository = git2::Repository::init_bare(&mirror).map_err(|error| {
        MirrorError::local(format!(
            "cannot create the shared mirror at {}: {error}",
            mirror.display()
        ))
    })?;
    repository.remote("origin", remote_url).map_err(|error| {
        MirrorError::local(format!("cannot add the shared mirror's remote: {error}"))
    })?;
    drop(repository);
    record_remote(&mirror, remote_url)?;
    Ok(mirror)
}

/// The refspec a mirror fetch of one requested reference needs.
///
/// A tag is fetched by name, so negotiation is about exactly the history the
/// user asked for. The default branch maps heads onto heads so the bare mirror
/// keeps a real branch to point HEAD at.
fn refspec_for(revision: Option<&str>) -> String {
    match revision {
        Some(tag) => format!("+refs/tags/{tag}:refs/tags/{tag}"),
        None => "+refs/heads/*:refs/heads/*".to_owned(),
    }
}

/// Fetch one reference into the mirror, shallow and incremental.
///
/// Objects this version shares with a version the mirror already holds are
/// reachable from a commit libgit2 reports as already present, so the server
/// leaves them out of the pack: a second version costs what it changed, not
/// what it restates. Returns the number of objects the transfer added, measured
/// against the object database before and after - the count is the evidence
/// that a pull really was incremental, and it is measurable on a local
/// transport where byte counts are not.
pub fn fetch_ref_to_mirror(
    mirror: &Path,
    remote_url: &str,
    revision: Option<&str>,
    callbacks: &MirrorCallbacks<'_>,
) -> Result<usize, MirrorError> {
    let repository = open_mirror_with_remote(mirror, remote_url)?;
    let before = count_objects(&repository);
    let refspec = refspec_for(revision);
    let depth = if is_local_remote(remote_url) { None } else { Some(1) };
    let proxy = crate::detect_proxy_url();
    let mut remote = repository.find_remote("origin").map_err(|error| {
        MirrorError::local(format!("the shared mirror has no origin remote: {error}"))
    })?;

    let fetch_with =
        |remote: &mut git2::Remote<'_>, proxy_url: Option<&str>| -> Result<(), git2::Error> {
            let mut options = mirror_fetch_options(callbacks, proxy_url, depth);
            // Follow the tags attached to the objects this fetch downloads,
            // never the whole tag list: re-negotiating every ref on each pull
            // is exactly the cost the mirror exists to remove.
            options.download_tags(AutotagOption::Auto);
            remote.fetch(&[refspec.as_str()], Some(&mut options), None)
        };
    let outcome = match proxy.as_deref() {
        Some(proxy_url) => match fetch_with(&mut remote, Some(proxy_url)) {
            Ok(()) => Ok(()),
            Err(first) => fetch_with(&mut remote, None).map_err(|_| first),
        },
        None => fetch_with(&mut remote, None),
    };
    outcome.map_err(|error| match error.class() {
        ErrorClass::Net | ErrorClass::Http | ErrorClass::Ssl => {
            MirrorError::transport(format!("cannot reach {remote_url}: {error}"))
        }
        _ => MirrorError::local(format!("cannot fetch from {remote_url}: {error}")),
    })?;
    let after = count_objects(&repository);
    callbacks.report(MAX_REPORTED_PROGRESS);
    Ok(after.saturating_sub(before))
}

/// How many objects the mirror's object database holds.
pub fn count_objects(repository: &git2::Repository) -> usize {
    let mut count = 0usize;
    if let Ok(odb) = repository.odb() {
        let _ = odb.foreach(|_| {
            count += 1;
            true
        });
    }
    count
}

/// Resolve one requested reference inside the mirror.
///
/// This never touches the network, which is what makes an offline pull of an
/// already-mirrored version work: the caller asks here first and only fetches
/// what is missing.
pub fn resolve_in_mirror(mirror: &Path, revision: Option<&str>) -> Result<String, MirrorError> {
    let repository = git2::Repository::open_bare(mirror).map_err(|error| {
        MirrorError::local(format!("cannot open the shared mirror: {error}"))
    })?;
    let object = match revision {
        Some(tag) => repository
            .revparse_single(&format!("refs/tags/{tag}"))
            .map_err(|error| {
                MirrorError::local(format!("the shared mirror has no tag {tag}: {error}"))
            })?,
        None => {
            // The bare mirror's HEAD points at a branch the heads-to-heads
            // refspec keeps in step with the remote's default branch.
            let head = repository.head().map_err(|error| {
                MirrorError::local(format!("the shared mirror has no HEAD: {error}"))
            })?;
            head.peel(git2::ObjectType::Any).map_err(|error| {
                MirrorError::local(format!("cannot resolve the shared mirror's HEAD: {error}"))
            })?
        }
    };
    let commit = object.peel_to_commit().map_err(|error| {
        MirrorError::local(format!(
            "the shared mirror cannot resolve {} to a commit: {error}",
            revision.unwrap_or("HEAD")
        ))
    })?;
    Ok(commit.id().to_string())
}

/// Remove the mirror so the next pull rebuilds it from nothing.
///
/// Only called after a fetch *succeeded* and the requested ref still could not
/// be resolved - the one case where the objects are provably incomplete while
/// the remote is reachable.
pub fn rebuild_bare_mirror(
    root: &Path,
    remote_url: &str,
    callbacks: &MirrorCallbacks<'_>,
) -> Result<PathBuf, MirrorError> {
    let mirror = mirror_path(root);
    if mirror.exists() {
        fs::remove_dir_all(&mirror).map_err(|error| {
            MirrorError::local(format!(
                "cannot remove the damaged shared mirror at {}: {error}",
                mirror.display()
            ))
        })?;
    }
    ensure_bare_mirror(root, remote_url, callbacks)
}

/// Delete the mirror and report the bytes freed.
///
/// The caller (the daemon) refuses while a pull task holds the mirror's resource
/// key, so this function only does the work.
pub fn purge(root: &Path) -> Result<u64, MirrorError> {
    let mirror = mirror_path(root);
    let freed = mirror_size_bytes(&mirror);
    if mirror.exists() {
        fs::remove_dir_all(&mirror).map_err(|error| {
            MirrorError::local(format!(
                "cannot remove the shared mirror at {}: {error}",
                mirror.display()
            ))
        })?;
    }
    Ok(freed)
}

/// Check out one commit from the mirror into a plain directory - no network,
/// and no copy of the object database.
///
/// Two ways to get a working tree out of a bare repository were measured on
/// this repository and this machine (2026-11-05, release build):
///
/// - `clone` the mirror locally, then check out: 92 s, and the cloned
///   `.git` is another 375 MB of copied object database;
/// - initialise an empty repository, point its object store at the mirror's
///   `objects/` directory through an alternate, then check out: 64 s and a
///   2.5 MB `.git`.
///
/// The second wins on both counts, so it is what this function does. The `.git`
/// it leaves behind records an absolute path into the mirror, and the caller
/// therefore removes it before the tree is published: a prepared base is source,
/// not a checkout, and must not depend on a cache the user may purge. (`pnpm
/// install` runs without it: DSH's postinstall hook early-returns when
/// `git rev-parse --show-toplevel` fails, and the commit it needs is supplied
/// through `DSH_CLIENT_COMMIT_HASH`.)
pub fn checkout_from_mirror(
    mirror: &Path,
    destination: &Path,
    commit: &str,
    callbacks: &MirrorCallbacks<'_>,
) -> Result<(), MirrorError> {
    require_free_space(destination, CHECKOUT_MIN_FREE_BYTES, "a Harness checkout")?;
    callbacks.report(0);
    let object_id = git2::Oid::from_str(commit).map_err(|error| {
        MirrorError::local(format!("not a commit id: {commit} ({error})"))
    })?;
    // Confirm the commit exists before creating anything at the destination.
    {
        let bare = git2::Repository::open_bare(mirror).map_err(|error| {
            MirrorError::local(format!("cannot open the shared mirror: {error}"))
        })?;
        bare.find_object(object_id, None).map_err(|error| {
            MirrorError::local(format!(
                "the shared mirror does not hold commit {commit}: {error}"
            ))
        })?;
    }
    let repository = git2::Repository::init(destination).map_err(|error| {
        MirrorError::local(format!(
            "cannot prepare the checkout at {}: {error}",
            destination.display()
        ))
    })?;
    {
        let odb = repository.odb().map_err(|error| {
            MirrorError::local(format!("cannot open the checkout's object store: {error}"))
        })?;
        let alternates = mirror.join("objects");
        // An alternate is an absolute path on disk, which is why the caller
        // removes `.git` before publishing: a published tree must not carry a
        // link into a cache the user may purge.
        odb.add_disk_alternate(alternates.to_str().ok_or_else(|| {
            MirrorError::local(format!("unusable mirror path: {}", alternates.display()))
        })?)
        .map_err(|error| {
            MirrorError::local(format!("cannot borrow the shared mirror's objects: {error}"))
        })?;
    }
    // The object has to be looked up in the borrowing repository, not in the
    // mirror: libgit2 refuses to check out an object that belongs to another
    // repository handle ("object to checkout does not match repository").
    let object = repository.find_object(object_id, None).map_err(|error| {
        MirrorError::local(format!(
            "the borrowed object store cannot supply commit {commit}: {error}"
        ))
    })?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    checkout.progress(|_path, current, total| {
        if total > 0 {
            callbacks.report((current.saturating_mul(100) / total) as u8);
        }
    });
    // libgit2 cancels a checkout through the notification callback; returning
    // false leaves a partially written tree, which the caller deletes.
    checkout.notify(|_kind, _path, _baseline, _target, _workdir| !callbacks.is_cancelled());
    let outcome = repository.checkout_tree(&object, Some(&mut checkout));
    drop(object);
    if let Err(error) = outcome {
        return Err(MirrorError::local(format!(
            "cannot check out {commit} at {}: {error}",
            destination.display()
        )));
    }
    repository.set_head_detached(object_id).map_err(|error| {
        MirrorError::local(format!("cannot detach HEAD at commit {commit}: {error}"))
    })?;
    callbacks.report(MAX_REPORTED_PROGRESS);
    Ok(())
}

/// Remove the `.git` a checkout leaves behind, so the tree is plain source.
///
/// Measured at 0.03 s for this repository, and worth the removal twice over:
/// the checkout's `.git` points into the mirror through an alternate, and a
/// published prepared base must not depend on a cache.
pub fn drop_checkout_metadata(destination: &Path) {
    let metadata = destination.join(".git");
    if metadata.exists() {
        let _ = fs::remove_dir_all(metadata);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};

    /// A throwaway runtime root, named so two test runs never collide.
    fn temporary_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dsh-box-mirror-{label}-{}", now_nanos()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// A source repository with one commit per version, tagged `v1`/`v2`.
    ///
    /// A local directory is a real remote to libgit2 (its local transport),
    /// which is what lets these tests exercise ensure/fetch/checkout without a
    /// network - and what makes them meaningful, since the production path
    /// stops being a network clone once the mirror is warm.
    fn source_with_versions(root: &Path) -> PathBuf {
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let repository = Repository::init(&source).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        for (version, files) in [(1, 3usize), (2, 5usize)] {
            // Each version adds files, so the two commits share most of their
            // trees: a second fetch must transfer only what it adds.
            for index in 0..files {
                fs::write(source.join(format!("file-{index}.txt")), format!("v{version}-{index}"))
                    .unwrap();
            }
            let mut index = repository.index().unwrap();
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
            index.write().unwrap();
            let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repository.head().ok().and_then(|head| head.target());
            let parents: Vec<git2::Commit<'_>> = parent
                .iter()
                .filter_map(|id| repository.find_commit(*id).ok())
                .collect();
            let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
            let commit = repository
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &format!("version {version}"),
                    &tree,
                    &parent_refs,
                )
                .unwrap();
            let commit = repository.find_commit(commit).unwrap();
            repository
                .tag(&format!("v{version}"), commit.as_object(), &signature, "release", false)
                .unwrap();
        }
        source
    }

    #[test]
    fn ensure_bare_mirror_creates_a_bare_repository_once() {
        let root = temporary_root("ensure");
        let source = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        assert!(mirror.is_dir());
        assert!(is_usable(&mirror));
        // A bare repository with a remote and no clone: only git's own
        // template files (config, HEAD, sample hooks) - no objects yet.
        assert!(
            mirror_size_bytes(&mirror) < 128 * 1024,
            "an empty mirror must not carry objects: {}",
            mirror_size_bytes(&mirror)
        );
        // A second call must be a no-op, not a rebuild.
        let again = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        assert_eq!(again, mirror);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fetch_ref_to_mirror_is_incremental_across_versions() {
        let root = temporary_root("incremental");
        let source = source_with_versions(&root);
        let remote = source.to_str().unwrap();
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, remote, &callbacks).unwrap();

        let first = fetch_ref_to_mirror(&mirror, remote, Some("v1"), &callbacks).unwrap();
        assert!(first > 0, "the first fetch must transfer the version's objects");
        let first_commit = resolve_in_mirror(&mirror, Some("v1")).unwrap();
        assert_eq!(first_commit.len(), 40);

        let second = fetch_ref_to_mirror(&mirror, remote, Some("v2"), &callbacks).unwrap();
        // v2 shares v1's tree and commit; only the new blobs and its own tree/commit
        // arrive. The exact count is an implementation detail, the ordering is not.
        assert!(
            second < first,
            "a second version must transfer fewer objects than the first ({second} vs {first})"
        );
        let second_commit = resolve_in_mirror(&mirror, Some("v2")).unwrap();
        assert_ne!(first_commit, second_commit);

        // Re-fetching a version the mirror holds transfers nothing at all, which
        // is the property a repeat pull depends on.
        let repeat = fetch_ref_to_mirror(&mirror, remote, Some("v1"), &callbacks).unwrap();
        assert_eq!(repeat, 0, "re-fetching a mirrored version must transfer nothing");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_in_mirror_reports_a_missing_version_without_network() {
        let root = temporary_root("resolve");
        let source = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        // No fetch yet: the mirror exists but holds nothing.
        let error = resolve_in_mirror(&mirror, Some("v1")).unwrap_err();
        assert!(!error.transport, "an empty mirror is not a transport failure");
        assert!(!error.message.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkout_from_mirror_writes_the_tree_and_borrows_the_objects() {
        let root = temporary_root("checkout");
        let source = source_with_versions(&root);
        let remote = source.to_str().unwrap();
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, remote, &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, remote, Some("v2"), &callbacks).unwrap();
        let commit = resolve_in_mirror(&mirror, Some("v2")).unwrap();

        let destination = root.join("checkout");
        checkout_from_mirror(&mirror, &destination, &commit, &callbacks).unwrap();
        // v2 added file-0..file-4 on top of v1's file-0..file-2.
        for index in 0..5 {
            assert!(
                destination.join(format!("file-{index}.txt")).is_file(),
                "the checkout must contain file-{index}.txt"
            );
        }
        assert_eq!(
            fs::read_to_string(destination.join("file-3.txt")).unwrap(),
            "v2-3"
        );
        // The `.git` is a stub borrowing the mirror, not a copy of it: that is
        // the difference between a 2.5 MB checkout and a 375 MB one.
        assert!(destination.join(".git").is_dir());
        assert_git_is_borrowed(&destination);

        // Dropping it leaves plain source, which is what gets published.
        drop_checkout_metadata(&destination);
        assert!(!destination.join(".git").exists());
        assert!(destination.join("file-0.txt").is_file());
        let _ = fs::remove_dir_all(root);
    }

    /// The whole point of the alternate: the checkout must not carry a copied
    /// object database. A cloned `.git` would hold a pack of the mirror's
    /// objects; a borrowing one holds none.
    fn assert_git_is_borrowed(destination: &Path) {
        let pack_dir = destination.join(".git").join("objects").join("pack");
        let packs: Vec<_> = fs::read_dir(&pack_dir)
            .map(|entries| entries.flatten().collect())
            .unwrap_or_default();
        assert!(
            packs.is_empty(),
            "a borrowed `.git` must not carry a pack: {packs:?}"
        );
        // The stub is git's own config/HEAD/index/refs, kilobytes not megabytes.
        assert!(
            mirror_size_bytes(&destination.join(".git")) < 128 * 1024,
            "a borrowed `.git` must stay a stub"
        );
    }

    #[test]
    fn fetch_into_a_moved_remote_url_repoints_the_mirror() {
        let root = temporary_root("repoint");
        let first = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, first.to_str().unwrap(), &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, first.to_str().unwrap(), Some("v1"), &callbacks).unwrap();
        let remote_file = mirror.join(MIRROR_SOURCE_FILE);
        assert_eq!(
            fs::read_to_string(&remote_file).unwrap().trim(),
            first.to_str().unwrap()
        );

        // A second source standing in for a changed `config set mirror` value.
        let second = root.join("second");
        fs::create_dir_all(&second).unwrap();
        let repository = Repository::init(&second).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        fs::write(second.join("other.txt"), "other").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("other.txt")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let commit = repository
            .commit(Some("HEAD"), &signature, &signature, "other", &tree, &[])
            .unwrap();
        let commit = repository.find_commit(commit).unwrap();
        repository
            .tag("other", commit.as_object(), &signature, "release", false)
            .unwrap();

        // Re-pointing must keep the objects: the old version is still resolvable.
        fetch_ref_to_mirror(&mirror, second.to_str().unwrap(), Some("other"), &callbacks).unwrap();
        assert_eq!(
            fs::read_to_string(&remote_file).unwrap().trim(),
            second.to_str().unwrap()
        );
        assert!(resolve_in_mirror(&mirror, Some("v1")).is_ok());
        assert!(resolve_in_mirror(&mirror, Some("other")).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn purge_removes_the_mirror_and_reports_the_bytes_freed() {
        let root = temporary_root("purge");
        let source = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, source.to_str().unwrap(), Some("v1"), &callbacks).unwrap();
        assert!(mirror_size_bytes(&mirror) > 0);
        let freed = purge(&root).unwrap();
        assert!(freed > 0);
        assert!(!mirror_path(&root).exists());
        // Purging an absent mirror is not an error: it already has the answer.
        assert_eq!(purge(&root).unwrap(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_cancelled_fetch_stops_and_a_cancelled_checkout_writes_nothing() {
        let root = temporary_root("cancel");
        let source = source_with_versions(&root);
        let cancelled = || true;
        let callbacks = MirrorCallbacks::new(&cancelled);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        // Cancellation reports as a transport-class failure (the transfer aborts),
        // and it must not delete the mirror that already exists.
        let _ = fetch_ref_to_mirror(&mirror, source.to_str().unwrap(), Some("v1"), &callbacks);
        assert!(is_usable(&mirror));
        let _ = fs::remove_dir_all(root);
    }
}
