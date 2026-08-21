//! Bundled Node/npm/pnpm runtime manifest discovery and structured access.

use box_foundation::BoxResult;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

const MANIFEST_FILE: &str = "runtime-manifest.json";

fn strip_verbatim_prefix(path: &str) -> String {
    // Windows extended-length paths start with `\\?\`; bundled Node's
    // module resolver doesn't accept them.
    path.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or_else(|| path.to_owned())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeManifest {
    pub target: String,
    pub node_version: String,
    pub pnpm_version: String,
    #[serde(default)]
    pub node_sha256: Option<String>,
    #[serde(default)]
    pub pnpm_integrity: Option<String>,
    pub node_entry: String,
    pub npm_entry: String,
    pub pnpm_entry: String,
    #[serde(default)]
    pub git_version: Option<String>,
    #[serde(default)]
    pub git_entry: Option<String>,
    #[serde(default)]
    pub git_sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedBundledRuntime {
    pub root: PathBuf,
    pub manifest: RuntimeManifest,
}

impl ResolvedBundledRuntime {
    pub fn from_repo_root(repo_root: &Path) -> BoxResult<Self> {
        let target = bundled_target();
        let root = repo_root.join("resources").join("runtime").join(&target);
        Self::from_path(&root)
    }

    pub fn from_path(root: &Path) -> BoxResult<Self> {
        let manifest_path = root.join(MANIFEST_FILE);
        let contents = fs::read_to_string(&manifest_path)
            .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
        let manifest: RuntimeManifest = serde_json::from_str(&contents)
            .map_err(|error| format!("invalid manifest {}: {error}", manifest_path.display()))?;
        Ok(Self { root: root.to_path_buf(), manifest })
    }

    fn clean(&self, entry: &Path) -> PathBuf {
        // Tauri's resource_dir returns verbatim `\\?\` paths on Windows;
        // bundled Node crashes with `EISDIR lstat 'D:'` when those reach
        // `Module._findPath`. Strip the prefix so the child sees a normal
        // absolute path.
        let as_string = entry.to_string_lossy().into_owned();
        let stripped = strip_verbatim_prefix(&as_string);
        PathBuf::from(stripped)
    }

    pub fn node_executable(&self) -> PathBuf { self.clean(&self.root.join(&self.manifest.node_entry)) }
    pub fn npm_script(&self) -> PathBuf { self.clean(&self.root.join(&self.manifest.npm_entry)) }
    pub fn pnpm_script(&self) -> PathBuf { self.clean(&self.root.join(&self.manifest.pnpm_entry)) }

    pub fn node_dir(&self) -> PathBuf {
        self.node_executable().parent().map(Path::to_path_buf).unwrap_or(self.root.clone())
    }

    pub fn pnpm_dir(&self) -> PathBuf {
        self.pnpm_script().parent().map(Path::to_path_buf).unwrap_or(self.root.clone())
    }

    /// Directory that contains the bundled `git` entry point (e.g. the
    /// `cmd/` directory inside PortableGit). Returns `None` when the
    /// runtime manifest was published without a `git` entry — the typical
    /// case for Linux until DSH Box CI produces its own bundle.
    pub fn git_dir(&self) -> Option<PathBuf> {
        let entry = self.manifest.git_entry.as_deref()?.trim();
        if entry.is_empty() {
            return None;
        }
        // The manifest entry (e.g. "cmd/git.exe") is relative to the git/
        // subdirectory of the runtime root, not to the root itself.
        let executable = self.clean(&self.root.join("git").join(entry));
        Some(executable.parent().map(Path::to_path_buf).unwrap_or(self.root.clone()))
    }
}

/// True when the current compilation target is Linux. Used by callers
/// that need to make runtime decisions based on the host platform —
/// for example, whether to allow a host-git passthrough fallback when
/// the bundled git distribution is absent.
pub fn target_is_linux() -> bool {
    std::env::consts::OS == "linux"
}

/// Locate a usable `git` executable on the host filesystem. The lookup
/// only runs on Linux targets (Windows always uses the bundled binary).
/// Resolution order:
///
/// 1. Walk the inherited `PATH` and pick the first directory that
///    contains a `git` executable. This honours the user's distro
///    package manager and homebrew-style installs without ever spawning
///    a shell.
/// 2. Fall back to the well-known FHS locations `/usr/bin/git` and
///    `/usr/local/bin/git` for hosts where `PATH` is sanitised.
///
/// `None` is returned when neither path yields an executable. The
/// caller is expected to surface this as a user-visible error — DSH
/// Box never shells out to `git` itself, so a missing host binary
/// only fails loudly when pnpm actually needs it.
pub fn resolve_host_git_dir() -> Option<PathBuf> {
    if !target_is_linux() {
        return None;
    }
    if let Some(path_var) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path_var) {
            let candidate = entry.join("git");
            if candidate.is_file() {
                return Some(entry);
            }
        }
    }
    for well_known in ["/usr/bin", "/usr/local/bin"] {
        let candidate = PathBuf::from(well_known).join("git");
        if candidate.is_file() {
            return Some(PathBuf::from(well_known));
        }
    }
    None
}

pub fn bundled_target() -> String {
    let os = match std::env::consts::OS {
        "windows" => "win",
        "macos" => "macos",
        "linux" => "linux",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => other
    };
    format!("{os}-{arch}")
}