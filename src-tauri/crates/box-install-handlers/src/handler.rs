//! Install handlers — each spec variant gets its own fetch
//! implementation. Handlers are stateless; they take a `TaskContext`
//! for progress logging and cancellation checks, a `staging` directory
//! to write into, and the runtime config (root directory, pnpm
//! toolchain, GitHub mirror).

use std::path::{Path, PathBuf};

use box_extensions::transfer::{archive_content_root, extract_extension_tarball};
use box_foundation::{mirror_url, read_config};
use box_runtime::shallow_clone_with_cancel;
use box_scheduler::TaskContext;

use crate::spec::{GitHost, InstallSpec, PathMode, WorkspaceProtocol};

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("runtime directory is not configured")]
    RuntimeUnset,
    #[error("invalid git URL `{0}`: {1}")]
    InvalidGit(String, String),
    #[error("workspace package `{0}` not declared in {1}")]
    WorkspaceUnknown(String, String),
    #[error("`runtime:` spec is not yet implemented")]
    RuntimeUnimplemented,
    #[error("alias target cannot be resolved: {0}")]
    AliasTargetInvalid(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("download error: {0}")]
    Download(String),
    #[error("handler returned no source root")]
    Empty,
}

/// Outcome of a successful fetch: the staging directory holds the
/// package source under `<staging>/source/`, and `source_root` is the
/// path inside staging to hand to `import_into_repository`. For
/// tarballs, this may be a deeper path if the tarball contained a
/// top-level directory wrapper.
pub struct InstallOutcome {
    pub source_root: PathBuf,
}

/// Single trait every concrete handler implements. Concrete handlers
/// live as private types — callers use [`handler_for`].
pub trait InstallHandler {
    fn fetch(&self, task: &TaskContext, staging: &Path) -> Result<InstallOutcome, InstallError>;
}

/// Build the right handler for a parsed spec. The handler tree is
/// shallow: aliases unwind one level (we refuse recursive aliases).
pub fn handler_for(spec: InstallSpec) -> Result<Box<dyn InstallHandler>, InstallError> {
    let resolved = match spec {
        InstallSpec::Alias { target, .. } => {
            // Aliases are resolved at fetch time: the alias name only
            // matters when the caller asks us to install into a
            // profile (so the entry lands under the alias name). For
            // fetch, the target's content is what we need.
            if matches!(*target, InstallSpec::Alias { .. }) {
                return Err(InstallError::AliasTargetInvalid(
                    "recursive aliases are not supported".to_owned(),
                ));
            }
            *target
        }
        other => other,
    };
    Ok(match resolved {
        InstallSpec::Registry {
            scope,
            name,
            version,
        } => Box::new(RegistryHandler {
            scope,
            name,
            version,
        }),
        InstallSpec::Git {
            host,
            url,
            ref_,
        } => Box::new(GitHandler { host, url, ref_ }),
        InstallSpec::LocalPath { path, mode } => Box::new(LocalPathHandler { path, mode }),
        InstallSpec::LocalTarball { path } => Box::new(LocalTarballHandler { path }),
        InstallSpec::RemoteTarball { url } => Box::new(RemoteTarballHandler { url }),
        InstallSpec::Workspace { name, protocol } => Box::new(WorkspaceHandler { name, protocol }),
        InstallSpec::Runtime { .. } => return Err(InstallError::RuntimeUnimplemented),
        InstallSpec::Alias { .. } => unreachable!("alias resolved above"),
    })
}

// ─── Registry → registry dist.tarball ────────────────────────────────────

struct RegistryHandler {
    scope: Option<String>,
    name: String,
    version: Option<String>,
}

impl InstallHandler for RegistryHandler {
    fn fetch(&self, task: &TaskContext, staging: &Path) -> Result<InstallOutcome, InstallError> {
        // Resolve the package version and `dist.tarball` URL from the
        // configured npm registry, download, and unpack. We deliberately
        // do NOT use `pnpm pack` here: pnpm 11's `pack` only packs the
        // local workspace directory — with a `package.json` present in
        // staging, pnpm silently packs staging ITSELF instead of the
        // requested registry package, so the "tarball" contains only a
        // fake manifest. Querying the registry API directly is
        // deterministic, respects `npm_config_registry`, and reuses the
        // same tarball download path as every other handler.
        let (display_name, version) = match (&self.scope, &self.name, &self.version) {
            (Some(scope), name, Some(version)) => (format!("@{scope}/{name}"), version.clone()),
            (Some(scope), name, None) => (format!("@{scope}/{name}"), "latest".to_owned()),
            (None, name, Some(version)) => (name.clone(), version.clone()),
            (None, name, None) => (name.clone(), "latest".to_owned()),
        };
        fetch_registry_tarball(task, &display_name, &version, staging)
    }
}

// ─── Git ─────────────────────────────────────────────────────────────────

struct GitHandler {
    #[allow(dead_code)]
    host: GitHost,
    url: String,
    ref_: Option<String>,
}

impl InstallHandler for GitHandler {
    fn fetch(&self, task: &TaskContext, staging: &Path) -> Result<InstallOutcome, InstallError> {
        let destination = staging.join("source");
        if destination.exists() {
            std::fs::remove_dir_all(&destination).ok();
        }
        std::fs::create_dir_all(&destination)?;
        let config = read_config().map_err(|_| InstallError::RuntimeUnset)?;
        let target = mirror_url(&self.url, config.github_mirror.as_deref());
        task.log(&format!("cloning Git repository {target}"));
        let cancelled = task.clone();
        shallow_clone_with_cancel(&target, &destination, self.ref_.as_deref(), move || {
            cancelled.cancelled()
        })
        .map_err(|e| InstallError::InvalidGit(self.url.clone(), e))?;
        Ok(InstallOutcome {
            source_root: destination,
        })
    }
}

// ─── Local path ──────────────────────────────────────────────────────────

struct LocalPathHandler {
    path: PathBuf,
    mode: PathMode,
}

impl InstallHandler for LocalPathHandler {
    fn fetch(&self, task: &TaskContext, staging: &Path) -> Result<InstallOutcome, InstallError> {
        let destination = staging.join("source");
        if destination.exists() {
            std::fs::remove_dir_all(&destination).ok();
        }
        match self.mode {
            PathMode::Copy => {
                task.log(&format!("copying local path {}", self.path.display()));
                copy_dir_recursive(&self.path, &destination)?;
            }
            PathMode::Link => {
                task.log(&format!("linking local path {}", self.path.display()));
                std::os::unix::fs::symlink(&self.path, &destination)?;
            }
        }
        Ok(InstallOutcome {
            source_root: destination,
        })
    }
}

// ─── Local tarball ───────────────────────────────────────────────────────

struct LocalTarballHandler {
    path: PathBuf,
}

impl InstallHandler for LocalTarballHandler {
    fn fetch(&self, task: &TaskContext, staging: &Path) -> Result<InstallOutcome, InstallError> {
        if !self.path.is_file() {
            return Err(InstallError::Download(format!(
                "tarball {} does not exist",
                self.path.display()
            )));
        }
        let destination = staging.join("source");
        if destination.exists() {
            std::fs::remove_dir_all(&destination).ok();
        }
        std::fs::create_dir_all(&destination)?;
        task.log(&format!("extracting local tarball {}", self.path.display()));
        extract_extension_tarball(&self.path, &destination).map_err(InstallError::Download)?;
        let content_root = archive_content_root(&destination).map_err(InstallError::Download)?;
        Ok(InstallOutcome {
            source_root: content_root,
        })
    }
}

// ─── Remote tarball ──────────────────────────────────────────────────────

struct RemoteTarballHandler {
    url: String,
}

impl InstallHandler for RemoteTarballHandler {
    fn fetch(&self, task: &TaskContext, staging: &Path) -> Result<InstallOutcome, InstallError> {
        let destination = staging.join("source");
        if destination.exists() {
            std::fs::remove_dir_all(&destination).ok();
        }
        std::fs::create_dir_all(&destination)?;
        task.log(&format!("downloading tarball {}", self.url));
        download_remote_tarball(&self.url, &destination)?;
        let content_root = archive_content_root(&destination).map_err(InstallError::Download)?;
        Ok(InstallOutcome {
            source_root: content_root,
        })
    }
}

// ─── Workspace protocol ──────────────────────────────────────────────────

struct WorkspaceHandler {
    name: String,
    #[allow(dead_code)]
    protocol: WorkspaceProtocol,
}

impl InstallHandler for WorkspaceHandler {
    fn fetch(&self, task: &TaskContext, staging: &Path) -> Result<InstallOutcome, InstallError> {
        // Workspace lookup requires `task.profile_dir` to point at the
        // profile whose `pnpm-workspace.yaml` declares the package.
        let profile_dir = task
            .profile_dir
            .as_ref()
            .ok_or_else(|| InstallError::WorkspaceUnknown(self.name.clone(), "<no profile>".to_owned()))?;
        let workspace_manifest = profile_dir.join("pnpm-workspace.yaml");
        if !workspace_manifest.is_file() {
            return Err(InstallError::WorkspaceUnknown(
                self.name.clone(),
                workspace_manifest.display().to_string(),
            ));
        }
        let raw = std::fs::read_to_string(&workspace_manifest)?;
        let workspace: serde_yaml::Value = serde_yaml::from_str(&raw).map_err(|e| {
            InstallError::Download(format!("cannot parse {}: {e}", workspace_manifest.display()))
        })?;
        let packages = workspace.get("packages").and_then(|v| v.as_sequence()).ok_or_else(|| {
            InstallError::WorkspaceUnknown(
                self.name.clone(),
                format!("{}: no `packages:` array", workspace_manifest.display()),
            )
        })?;
        let entry = packages
            .iter()
            .find_map(|value| {
                let raw = value.as_str()?;
                let stem = raw
                    .trim_start_matches("./")
                    .trim_end_matches("/**")
                    .trim_end_matches('*')
                    .trim_end_matches('/');
                if stem == self.name || stem.ends_with(&format!("/{0}", self.name)) {
                    Some(stem.to_owned())
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                InstallError::WorkspaceUnknown(
                    self.name.clone(),
                    workspace_manifest.display().to_string(),
                )
            })?;
        let resolved = if entry.starts_with('/') {
            PathBuf::from(&entry)
        } else {
            profile_dir.join(&entry)
        };
        if !resolved.is_dir() {
            return Err(InstallError::WorkspaceUnknown(
                self.name.clone(),
                format!("resolved to {} which does not exist", resolved.display()),
            ));
        }
        task.log(&format!(
            "workspace package `{name}` → {path}",
            name = self.name,
            path = resolved.display()
        ));
        let destination = staging.join("source");
        if destination.exists() {
            std::fs::remove_dir_all(&destination).ok();
        }
        copy_dir_recursive(&resolved, &destination)?;
        Ok(InstallOutcome {
            source_root: destination,
        })
    }
}

// ─── Shared helpers ──────────────────────────────────────────────────────

/// Resolve `name@version` against the configured npm registry and
/// download+unpack the published tarball into `staging/source`.
///
/// We query `<registry>/<escaped-name>/<version>` (npm's abbreviated
/// metadata endpoint) for `dist.tarball`, then stream it to disk via the
/// same path all other tarball handlers use. A missing version falls
/// back to `latest`.
fn fetch_registry_tarball(
    task: &TaskContext,
    name: &str,
    version: &str,
    staging: &Path,
) -> Result<InstallOutcome, InstallError> {
    let registry = registry_base_url();
    let encoded_name = name.replace('/', "%2f");
    let url = format!("{registry}{encoded_name}/{version}");
    task.log(&format!("resolving {name}@{version} from npm registry"));
    let body = reqwest::blocking::get(&url)
        .map_err(|e| InstallError::Download(format!("GET {url}: {e}")))?
        .error_for_status()
        .map_err(|e| InstallError::Download(format!("GET {url}: {e}")))?
        .text()
        .map_err(|e| InstallError::Download(format!("cannot read registry metadata body for {name}: {e}")))?;
    let metadata: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| InstallError::Download(format!("cannot parse registry metadata for {name}: {e}")))?;
    let tarball = metadata
        .pointer("/dist/tarball")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            InstallError::Download(format!(
                "registry metadata for {name}@{version} has no dist.tarball"
            ))
        })?;
    let resolved_version = metadata
        .pointer("/version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(version);

    let destination = staging.join("source");
    if destination.exists() {
        std::fs::remove_dir_all(&destination).ok();
    }
    std::fs::create_dir_all(&destination)?;
    let archive = staging.join("package.tgz");
    task.log(&format!(
        "downloading {name}@{resolved_version} tarball"
    ));
    let mut response = reqwest::blocking::get(tarball)
        .map_err(|e| InstallError::Download(format!("GET {tarball}: {e}")))?;
    if !response.status().is_success() {
        return Err(InstallError::Download(format!(
            "GET {tarball}: HTTP {}",
            response.status()
        )));
    }
    let mut file = std::fs::File::create(&archive)?;
    std::io::copy(&mut response, &mut file)?;
    drop(file);
    extract_extension_tarball(&archive, &destination).map_err(InstallError::Download)?;
    let content_root = archive_content_root(&destination).map_err(InstallError::Download)?;
    Ok(InstallOutcome {
        source_root: content_root,
    })
}

/// npm registry base URL. Honors `npm_config_registry` when set
/// (DSH Box sets it in container profiles and via Settings); falls
/// back to the public npm registry.
fn registry_base_url() -> String {
    if let Ok(configured) = std::env::var("npm_config_registry") {
        let trimmed = configured.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return format!("{trimmed}/");
        }
    }
    "https://registry.npmjs.org/".to_owned()
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), InstallError> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | "__pycache__" | "target" | ".pnpm-store" | ".cache" | ".pnpm" | ".turbo" | ".next" | ".DS_Store"
            ) {
                continue;
            }
            copy_dir_recursive(&from, &target)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(&target, &target)?;
        } else {
            std::fs::copy(&from, &target)?;
        }
    }
    Ok(())
}

fn download_remote_tarball(url: &str, destination: &Path) -> Result<(), InstallError> {
    let archive_path = destination.with_extension("tgz");
    let mut response = reqwest::blocking::get(url)
        .map_err(|e| InstallError::Download(format!("GET {url}: {e}")))?;
    if !response.status().is_success() {
        return Err(InstallError::Download(format!(
            "GET {url}: HTTP {}",
            response.status()
        )));
    }
    let mut file = std::fs::File::create(&archive_path)?;
    std::io::copy(&mut response, &mut file)?;
    drop(file);
    extract_extension_tarball(&archive_path, destination).map_err(InstallError::Download)?;
    Ok(())
}