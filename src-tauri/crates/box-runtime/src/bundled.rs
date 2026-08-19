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