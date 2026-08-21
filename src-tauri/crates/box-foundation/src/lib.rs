//! Filesystem, configuration, and validation primitives shared by DSH Box features.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub type BoxResult<T> = Result<T, String>;

/// The forward-only on-disk layout used by prepared bases, sealed templates,
/// plugin artifacts, and physical container copies. This intentionally does
/// not read or repair the retired shared `runtimes/` layout.
pub const STORAGE_SCHEMA_VERSION: u32 = 10;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StorageSchema {
    pub schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PublishedResourceKind {
    PreparedBase,
    SealedTemplate,
    PluginArtifact,
    Container,
}

/// Provenance for one immutable plugin tarball. The source checkout is
/// optional because an artifact imported directly from a tarball need not
/// retain a second source copy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginArtifactManifest {
    pub schema_version: u32,
    pub id: String,
    pub package_name: String,
    pub package_version: String,
    pub artifact_digest: String,
    pub source: String,
    #[serde(default)]
    pub source_commit: Option<String>,
    #[serde(default)]
    pub lifecycle_approved: bool,
    pub created_at: u64,
}

/// Manifest shared by immutable prepared bases and sealed templates. The
/// manifest deliberately contains only relative resource identity, never an
/// absolute path that could turn a copied container back into a shared tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TemplateManifest {
    pub schema_version: u32,
    pub kind: PublishedResourceKind,
    pub id: String,
    pub source_ref: String,
    pub source_commit: String,
    pub node_version: String,
    pub pnpm_version: String,
    #[serde(default)]
    pub base_id: Option<String>,
    #[serde(default)]
    pub plugin_artifact_ids: Vec<String>,
    /// Original pnpm source specs accepted by `dsh plugin add`. These are
    /// retained for display and recipe identity; the authoritative resolved
    /// package graph lives in the profile lockfile shipped with a sealed
    /// template.
    #[serde(default)]
    pub plugin_sources: Vec<String>,
    pub harness_digest: String,
    #[serde(default)]
    pub profile_digest: Option<String>,
    pub size_bytes: u64,
    pub validated_at: u64,
}

impl TemplateManifest {
    pub fn validate(&self) -> BoxResult<()> {
        if !matches!(
            self.kind,
            PublishedResourceKind::PreparedBase | PublishedResourceKind::SealedTemplate
        ) {
            return Err(
                "template manifest must describe a prepared base or sealed template".to_owned(),
            );
        }
        if self.schema_version != STORAGE_SCHEMA_VERSION {
            return Err(format!(
                "template manifest schema {} is unsupported; expected {STORAGE_SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        for (label, value) in [
            ("id", self.id.as_str()),
            ("harness digest", self.harness_digest.as_str()),
        ] {
            if value.is_empty() {
                return Err(format!("template manifest {label} cannot be empty"));
            }
        }
        match self.kind {
            PublishedResourceKind::PreparedBase if self.base_id.is_some() => {
                Err("prepared base must not reference another base".to_owned())
            }
            PublishedResourceKind::SealedTemplate
                if self.base_id.as_deref().unwrap_or_default().is_empty() =>
            {
                Err("sealed template must reference a prepared base".to_owned())
            }
            _ => Ok(()),
        }
    }
}

/// Canonical locations for the schema-10 runtime root. Keeping these paths in
/// foundation prevents CLI, daemon, and desktop adapters from independently
/// recreating a subtly different layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLayout {
    root: PathBuf,
}

impl RuntimeLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    pub fn storage_path(&self) -> PathBuf {
        self.state_dir().join("storage.json")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    pub fn repository_dir(&self) -> PathBuf {
        self.root.join("repository")
    }

    pub fn plugin_artifacts_dir(&self) -> PathBuf {
        self.repository_dir().join("plugins")
    }

    pub fn templates_dir(&self) -> PathBuf {
        self.root.join("templates")
    }

    pub fn prepared_base_dir(&self, digest: &str) -> BoxResult<PathBuf> {
        validated_digest_path(self.templates_dir(), "base", digest)
    }

    pub fn sealed_template_dir(&self, digest: &str) -> BoxResult<PathBuf> {
        validated_digest_path(self.templates_dir(), "sealed", digest)
    }

    pub fn container_dir(&self, id: &str) -> BoxResult<PathBuf> {
        validated_digest_path(self.root.join("instances"), "container", id)
    }

    /// Creates a unique task-private directory. Published resources must be
    /// built here and moved only through `publish_staged_tree`.
    pub fn create_staging_dir(&self, task_id: &str) -> BoxResult<PathBuf> {
        if !is_safe_identifier(task_id) {
            return Err(format!("unsafe staging task id `{task_id}`"));
        }
        let parent = self.staging_dir();
        fs::create_dir_all(&parent).map_err(|error| error.to_string())?;
        for attempt in 0..1000_u32 {
            let candidate = parent.join(format!("{task_id}-{}-{attempt}", now_nanos()));
            match fs::create_dir(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("cannot create staging directory: {error}")),
            }
        }
        Err("cannot allocate unique staging directory".to_owned())
    }

    /// Creates schema-10 bookkeeping for a fresh root. A root that contains
    /// the retired shared-runtime layout is rejected so callers can present a
    /// recovery/new-root choice rather than silently mixing two models.
    pub fn initialize_schema_10(&self) -> BoxResult<()> {
        let legacy_runtime = self.root.join("runtimes");
        if legacy_runtime.exists() {
            return Err(format!(
                "runtime root {} uses the unsupported pre-schema-10 `runtimes` layout; select a new empty runtime directory",
                self.root.display()
            ));
        }
        fs::create_dir_all(self.state_dir()).map_err(|error| error.to_string())?;
        fs::create_dir_all(self.staging_dir()).map_err(|error| error.to_string())?;
        fs::create_dir_all(self.plugin_artifacts_dir()).map_err(|error| error.to_string())?;
        fs::create_dir_all(self.templates_dir()).map_err(|error| error.to_string())?;
        fs::create_dir_all(self.root.join("instances")).map_err(|error| error.to_string())?;
        match fs::read_to_string(self.storage_path()) {
            Ok(text) => {
                let schema: StorageSchema = serde_json::from_str(&text)
                    .map_err(|error| format!("cannot parse storage schema: {error}"))?;
                if schema.schema_version != STORAGE_SCHEMA_VERSION {
                    return Err(format!(
                        "runtime root uses storage schema {}; expected schema {STORAGE_SCHEMA_VERSION}",
                        schema.schema_version
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                atomic_write_json(
                    &self.storage_path(),
                    &StorageSchema {
                        schema_version: STORAGE_SCHEMA_VERSION,
                    },
                )?;
            }
            Err(error) => return Err(format!("cannot read storage schema: {error}")),
        }
        Ok(())
    }

    /// Atomically publishes a complete staged tree. Destination replacement is
    /// forbidden: digest-addressed resources are immutable and a retry must
    /// use a new staging tree rather than mutate a published resource.
    pub fn publish_staged_tree(&self, staged: &Path, destination: &Path) -> BoxResult<()> {
        let staging_root = self.staging_dir();
        let relative = staged.strip_prefix(&staging_root).map_err(|_| {
            format!(
                "refusing to publish non-staging path {}; expected a child of {}",
                staged.display(),
                staging_root.display()
            )
        })?;
        if relative.as_os_str().is_empty() || !staged.is_dir() {
            return Err("staged resource must be a non-empty directory".to_owned());
        }
        if destination.exists() {
            return Err(format!(
                "refusing to overwrite published resource {}",
                destination.display()
            ));
        }
        let parent = destination
            .parent()
            .ok_or("published resource has no parent directory")?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        fs::rename(staged, destination).map_err(|error| {
            format!(
                "cannot atomically publish {} to {}: {error}",
                staged.display(),
                destination.display()
            )
        })
    }
}

fn validated_digest_path(parent: PathBuf, prefix: &str, value: &str) -> BoxResult<PathBuf> {
    if !is_safe_identifier(value) {
        return Err(format!("unsafe resource identifier `{value}`"));
    }
    Ok(parent.join(format!("{prefix}-{value}")))
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Write JSON through a same-directory temporary file so readers never see a
/// partially written schema or manifest.
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> BoxResult<()> {
    let parent = path.parent().ok_or("JSON path has no parent directory")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let body = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("JSON path has no UTF-8 file name")?;
    let temporary = parent.join(format!(".{file_name}.{}.tmp", now_nanos()));
    fs::write(&temporary, body).map_err(|error| error.to_string())?;
    atomic_replace_file(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("cannot atomically replace {}: {error}", path.display())
    })
}

#[cfg(not(windows))]
fn atomic_replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{iter, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Validate and write the manifest that makes a staged base/template eligible
/// for publication. Callers must write it before `publish_staged_tree`.
pub fn write_template_manifest(
    staged_directory: &Path,
    manifest: &TemplateManifest,
) -> BoxResult<()> {
    if !staged_directory.is_dir() {
        return Err(format!(
            "cannot write manifest into missing staging directory {}",
            staged_directory.display()
        ));
    }
    manifest.validate()?;
    atomic_write_json(&staged_directory.join("manifest.json"), manifest)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoxConfig {
    pub runtime_directory: Option<String>,
    pub selected_dsh_version: Option<String>,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub toolchain_sources: BTreeMap<String, String>,
    /// Optional GitHub mirror prefix (e.g. `https://gh-proxy.com`). When set,
    /// GitHub URLs are rewritten as `<mirror>/<original-url>` for the version
    /// catalog, runtime clones, and extension imports.
    #[serde(default)]
    pub github_mirror: Option<String>,
    /// Optional npm registry (e.g. `https://registry.npmmirror.com`). When
    /// set, spawned pnpm/npm toolchains receive `npm_config_registry`.
    #[serde(default)]
    pub npm_registry: Option<String>,
    /// Optional absolute path to a `git` executable the user wants DSH Box
    /// to use. Linux defaults to the host's PATH-discovered git; this
    /// override lets a developer point Box at a custom build (e.g.
    /// /opt/myteam/bin/git) without exporting PATH globally.
    #[serde(default)]
    pub git_path: Option<String>,
    /// When false (the default), DSH Box inherits the host's
    /// HTTP/HTTPS/NO_PROXY variables into the clean-room package-manager
    /// child so developers behind a corporate mirror can still reach
    /// GitHub and the npm registry. Set to true to force a fully
    /// hermetic child (no proxy of any kind).
    #[serde(default = "default_true")]
    pub inherit_proxy: bool,
    /// SHA-256 of the bundled plugins manifest Box wrote into
    /// `<runtimeDirectory>/plugins/node_modules/` on the last vendor pass.
    /// `initialize_bundled_plugins` compares this against the current
    /// resource manifest and skips the copy when they match.
    #[serde(default)]
    pub plugins_manifest_digest: Option<String>,
    /// Windows only: `"<install-dir>|<runtime-dir>"` pair whose Defender
    /// real-time-scan exclusions were already registered (or found
    /// unavailable). The desktop shell re-prompts via UAC only when the
    /// current pair differs, so changing the runtime directory triggers
    /// exactly one new elevation prompt.
    #[serde(default)]
    pub defender_exclusions_for: Option<String>,
}

fn default_language() -> String {
    "en".to_owned()
}

fn default_true() -> bool {
    true
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            runtime_directory: None,
            selected_dsh_version: None,
            language: default_language(),
            toolchain_sources: BTreeMap::new(),
            github_mirror: None,
            npm_registry: None,
            git_path: None,
            inherit_proxy: default_true(),
            plugins_manifest_digest: None,
            defender_exclusions_for: None,
        }
    }
}

/// Canonical paths for user configuration and runtime-managed data.
#[derive(Debug, Clone)]
pub struct BoxPaths {
    pub config: PathBuf,
    pub runtime: Option<PathBuf>,
}

impl BoxPaths {
    pub fn from_config(config: &BoxConfig) -> BoxResult<Self> {
        Ok(Self {
            config: config_path()?,
            runtime: config.runtime_directory.as_ref().map(PathBuf::from),
        })
    }
    pub fn tasks_state(&self) -> BoxResult<PathBuf> {
        Ok(self
            .runtime
            .as_ref()
            .ok_or("DSH Box storage is not configured")?
            .join("state/tasks.json"))
    }
    pub fn task_log(&self, id: &str) -> BoxResult<PathBuf> {
        Ok(self
            .runtime
            .as_ref()
            .ok_or("DSH Box storage is not configured")?
            .join("logs/tasks")
            .join(format!("{id}.log")))
    }

    /// `<runtime>/logs/` — the unified tracing destination for every
    /// subsystem (`daemon.log`, `desktop.log`, `cli.log`, `bundled.log`).
    /// Created on demand by the logger crate.
    pub fn log_dir(&self) -> BoxResult<PathBuf> {
        Ok(self
            .runtime
            .as_ref()
            .ok_or("DSH Box storage is not configured")?
            .join("logs"))
    }
}

pub fn config_path() -> BoxResult<PathBuf> {
    // `DSHBOX_CONFIG_DIR` lets tests and CLI invocations point at a
    // sandboxed config without mutating HOME/USERPROFILE. This is the
    // same env var name the dispatch-test harness sets, so the production
    // path stays one lookup.
    if let Some(override_dir) = std::env::var_os("DSHBOX_CONFIG_DIR") {
        return Ok(PathBuf::from(override_dir).join("config.json"));
    }
    Ok(dirs::home_dir()
        .ok_or("cannot determine home directory")?
        .join(".dsh-box/config.json"))
}
pub fn read_config() -> BoxResult<BoxConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(BoxConfig::default());
    }
    let mut config: BoxConfig = serde_json::from_str(
        &fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    config.toolchain_sources.remove("git");
    if let Some(directory) = config.runtime_directory.take() {
        // Heal configurations persisted before drive-root normalization so
        // legacy `D:` values never reach downstream path joins.
        config.runtime_directory = normalize_runtime_directory(&directory)
            .ok()
            .or(Some(directory));
    }
    Ok(config)
}
pub fn write_config(config: &BoxConfig) -> BoxResult<()> {
    let path = config_path()?;
    fs::create_dir_all(path.parent().ok_or("configuration path has no parent")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        path,
        serde_json::to_string_pretty(config).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}
/// Resolves a user-selected directory to a canonical absolute path suitable
/// for persistence. Windows drive roots (`D:`) are drive-relative: joining
/// them with child names yields `D:containers`, which resolves against each
/// child process's current drive and crashes bundled Node/pnpm. Normalize
/// before saving so every later `PathBuf::join` stays on an absolute root.
/// Existing relative entries are resolved against the current directory.
pub fn normalize_runtime_directory(directory: &str) -> BoxResult<String> {
    let mut selected = PathBuf::from(directory);
    #[cfg(windows)]
    if drive_root(&selected) {
        selected.push("\\");
    }
    if !selected.is_absolute() {
        selected = env::current_dir()
            .map_err(|error| error.to_string())?
            .join(selected);
    }
    let canonical = fs::canonicalize(&selected).map_err(|error| {
        format!(
            "cannot resolve runtime directory {}: {error}",
            selected.display()
        )
    })?;
    Ok(strip_verbatim_prefix(&canonical.to_string_lossy()))
}

/// True for drive-relative roots like `D:`, where a bare trailing colon makes
/// every joined path resolve against the drive's current directory.
#[cfg(windows)]
fn drive_root(path: &PathBuf) -> bool {
    let bytes = path.to_string_lossy();
    let bytes = bytes.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// `std::fs::canonicalize` returns verbatim `\\?\` prefixed paths on Windows.
/// Strip the prefix so stored and displayed paths stay ordinary while they
/// remain absolute for every resolver.
///
/// Public because Tauri's `resource_dir()` also returns verbatim paths on
/// Windows, and bundled Node crashes with `EISDIR lstat 'D:'` when handed a
/// verbatim entry script, so spawned tool paths must be stripped too.
pub fn strip_verbatim_prefix(path: &str) -> String {
    #[cfg(windows)]
    {
        path.strip_prefix("\\\\?\\UNC\\")
            .map(|rest| format!("\\\\{rest}"))
            .or_else(|| path.strip_prefix("\\\\?\\").map(str::to_owned))
            .unwrap_or_else(|| path.to_owned())
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Rewrites a URL through a user-configured mirror. Two shapes are accepted:
///
/// 1. **Proxy prefix** such as `https://gh-proxy.com`: the original URL is
///    appended after a slash (`<proxy>/<original-url>`), which is how common
///    GitHub accelerators expose both the web UI and the REST API.
/// 2. **Direct host** such as `https://api.github.com` or `https://github.com`:
///    treated as "no mirror" and the URL passes through unchanged. The
///    upstream URLs already use these hosts, so a prefix would duplicate the
///    authority (`https://github.com/https://api.github.com/...`) and 404.
///
/// An empty or absent mirror leaves the URL unchanged.
pub fn mirror_url(url: &str, mirror: Option<&str>) -> String {
    let Some(mirror) = mirror.map(str::trim).filter(|value| !value.is_empty()) else {
        return url.to_owned();
    };
    let mirror = mirror.trim_end_matches('/');
    if matches!(extract_host(mirror), Some(host) if is_official_github_host(host)) {
        return url.to_owned();
    }
    format!("{mirror}/{url}")
}

/// Pulls the host portion out of a `scheme://host[/path]` value. Accepts a
/// bare hostname (`gh-proxy.com` or `api.github.com`) too; in that case the
/// whole value is the host.
fn extract_host(value: &str) -> Option<&str> {
    let after_scheme = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(value);
    Some(after_scheme.split('/').next().unwrap_or(after_scheme))
}

/// Hosts that already back GitHub web/API traffic. Pointing the mirror at one
/// of them is a misconfiguration (the original URLs are absolute), so the
/// request is passed through unchanged.
fn is_official_github_host(host: &str) -> bool {
    matches!(
        host,
        "github.com"
            | "api.github.com"
            | "raw.githubusercontent.com"
            | "codeload.github.com"
            | "objects.githubusercontent.com"
            | "gist.github.com"
    )
}

/// Normalizes a user-entered mirror/registry value: trims whitespace and maps
/// an empty string to `None`, so clearing the field disables the setting.
pub fn normalize_optional_url(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Prevents a child process from opening a console window on Windows. The
/// desktop app is a GUI process without a console; spawned console children
/// (node, pnpm, schtasks, ...) would otherwise each pop a black terminal
/// window. No-op on other platforms.
pub fn suppress_console_window(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}
pub fn is_safe_identifier(value: &str) -> bool {
    value == "latest"
        || (!value.is_empty()
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
            }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temporary_root(label: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("dshbox-foundation-{label}-{}", now_nanos()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn schema_10_initialization_creates_only_new_layout() {
        let root = temporary_root("schema");
        let layout = RuntimeLayout::new(&root);
        layout.initialize_schema_10().unwrap();
        let stored: StorageSchema =
            serde_json::from_str(&fs::read_to_string(layout.storage_path()).unwrap()).unwrap();
        assert_eq!(stored.schema_version, STORAGE_SCHEMA_VERSION);
        assert!(layout.staging_dir().is_dir());
        assert!(layout.plugin_artifacts_dir().is_dir());
        assert!(layout.templates_dir().is_dir());
        assert!(!root.join("runtimes").exists());
        // A second daemon start reads and rewrites the same schema file. On
        // Windows this specifically exercises MOVEFILE_REPLACE_EXISTING.
        layout.initialize_schema_10().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_10_rejects_legacy_runtime_without_mutating_it() {
        let root = temporary_root("legacy");
        let legacy = root.join("runtimes").join("latest").join("source");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("keep.txt"), "preserve me").unwrap();
        let error = RuntimeLayout::new(&root)
            .initialize_schema_10()
            .unwrap_err();
        assert!(error.contains("unsupported pre-schema-10"));
        assert_eq!(
            fs::read_to_string(legacy.join("keep.txt")).unwrap(),
            "preserve me"
        );
        assert!(!root.join("state/storage.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_tree_is_published_once_without_overwrite() {
        let root = temporary_root("publish");
        let layout = RuntimeLayout::new(&root);
        layout.initialize_schema_10().unwrap();
        let staged = layout.create_staging_dir("template_build").unwrap();
        fs::write(staged.join("manifest.json"), "complete").unwrap();
        let destination = layout.sealed_template_dir("abc123").unwrap();
        layout.publish_staged_tree(&staged, &destination).unwrap();
        assert!(!staged.exists());
        assert_eq!(
            fs::read_to_string(destination.join("manifest.json")).unwrap(),
            "complete"
        );
        let retry = layout.create_staging_dir("template_build").unwrap();
        assert!(layout.publish_staged_tree(&retry, &destination).is_err());
        assert!(retry.is_dir(), "failed publish keeps retry staging intact");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sealed_manifest_requires_base_and_is_written_atomically() {
        let root = temporary_root("manifest");
        let layout = RuntimeLayout::new(&root);
        layout.initialize_schema_10().unwrap();
        let staged = layout.create_staging_dir("template_build").unwrap();
        let invalid = TemplateManifest {
            schema_version: STORAGE_SCHEMA_VERSION,
            kind: PublishedResourceKind::SealedTemplate,
            id: "sealed-1".to_owned(),
            source_ref: "github.com/deepseek-ai/deepseek-harness:latest".to_owned(),
            source_commit: "commit".to_owned(),
            node_version: "24".to_owned(),
            pnpm_version: "10".to_owned(),
            base_id: None,
            plugin_artifact_ids: Vec::new(),
            plugin_sources: Vec::new(),
            harness_digest: "digest".to_owned(),
            profile_digest: Some("profile".to_owned()),
            size_bytes: 1,
            validated_at: now_seconds(),
        };
        assert!(write_template_manifest(&staged, &invalid).is_err());
        let valid = TemplateManifest {
            base_id: Some("base-1".to_owned()),
            ..invalid
        };
        write_template_manifest(&staged, &valid).unwrap();
        let reread: TemplateManifest =
            serde_json::from_str(&fs::read_to_string(staged.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(reread, valid);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drive_root_normalizes_to_absolute() {
        // `C:` must become a rooted path, never a drive-relative one that
        // later joins (`C:containers`) resolve against each child's drive.
        #[cfg(windows)]
        {
            let drive = env::temp_dir()
                .to_string_lossy()
                .chars()
                .next()
                .expect("temp dir starts with a drive letter")
                .to_string();
            let input = format!("{drive}:");
            let result = normalize_runtime_directory(&input).unwrap();
            let path = PathBuf::from(&result);
            assert!(path.is_absolute(), "must be absolute: {result}");
            assert!(
                !result.ends_with(':'),
                "must not stay drive-relative: {result}"
            );
        }
        #[cfg(not(windows))]
        {
            let result = normalize_runtime_directory(".").unwrap();
            assert!(PathBuf::from(&result).is_absolute());
        }
    }

    #[test]
    fn existing_absolute_path_is_canonicalized() {
        let temp = env::temp_dir().join("dsh-box-normalize-preserve");
        fs::create_dir_all(&temp).unwrap();
        let input = temp.to_string_lossy().into_owned();
        let result = normalize_runtime_directory(&input).unwrap();
        let expected = fs::canonicalize(&temp).unwrap();
        #[cfg(windows)]
        let expected = {
            // The stored value intentionally drops the verbatim prefix that
            // `std::fs::canonicalize` adds on Windows.
            PathBuf::from(without_verbatim(&expected))
        };
        assert_eq!(
            PathBuf::from(&result),
            expected,
            "stored path must match the canonical path"
        );
        fs::remove_dir_all(&temp).ok();
    }

    #[cfg(windows)]
    fn without_verbatim(path: &Path) -> String {
        let text = path.to_string_lossy();
        text.strip_prefix("\\\\?\\UNC\\")
            .map(|rest| format!("\\\\{rest}"))
            .or_else(|| text.strip_prefix("\\\\?\\").map(str::to_owned))
            .unwrap_or_else(|| text.into_owned())
    }

    #[test]
    fn nonexistent_directory_is_rejected() {
        let missing = env::temp_dir().join("dsh-box-normalize-missing-xyz");
        assert!(normalize_runtime_directory(&missing.to_string_lossy()).is_err());
    }

    #[test]
    fn mirror_url_passthrough_when_unset() {
        assert_eq!(
            mirror_url("https://api.github.com/repos/x/y/tags", None),
            "https://api.github.com/repos/x/y/tags"
        );
        assert_eq!(
            mirror_url("https://api.github.com/repos/x/y/tags", Some("")),
            "https://api.github.com/repos/x/y/tags"
        );
        assert_eq!(
            mirror_url("https://api.github.com/repos/x/y/tags", Some("   ")),
            "https://api.github.com/repos/x/y/tags"
        );
    }

    #[test]
    fn mirror_url_passthrough_for_official_github_hosts() {
        // The user pointed the mirror at GitHub itself; the original URLs
        // already use these hosts, so a prefix would duplicate the
        // authority and produce a 404.
        let url = "https://api.github.com/repos/deepseek-ai/deepseek-harness/tags?per_page=100";
        for mirror in [
            "https://api.github.com",
            "https://github.com",
            "http://github.com",
            "api.github.com",
            "  https://github.com/  ",
        ] {
            assert_eq!(mirror_url(url, Some(mirror)), url, "mirror: {mirror}");
        }
    }

    #[test]
    fn mirror_url_prefix_for_proxy() {
        // Third-party accelerators expect `<proxy>/<original-url>`.
        assert_eq!(
            mirror_url(
                "https://api.github.com/repos/x/y/tags?per_page=100",
                Some("https://gh-proxy.com")
            ),
            "https://gh-proxy.com/https://api.github.com/repos/x/y/tags?per_page=100"
        );
        // Trailing slashes are stripped so the result is well-formed.
        assert_eq!(
            mirror_url(
                "https://github.com/owner/repo.git",
                Some("https://gh-proxy.com/")
            ),
            "https://gh-proxy.com/https://github.com/owner/repo.git"
        );
        // Bare hostnames (no scheme) are still treated as a proxy.
        assert_eq!(
            mirror_url("https://github.com/owner/repo.git", Some("gh-proxy.com")),
            "gh-proxy.com/https://github.com/owner/repo.git"
        );
    }
}
