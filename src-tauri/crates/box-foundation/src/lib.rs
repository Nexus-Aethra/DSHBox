//! Filesystem, configuration, and validation primitives shared by DSH Box features.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub type BoxResult<T> = Result<T, String>;

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
    /// SHA-256 of the bundled plugins manifest Box wrote into
    /// `<runtimeDirectory>/plugins/node_modules/` on the last vendor pass.
    /// `initialize_bundled_plugins` compares this against the current
    /// resource manifest and skips the copy when they match.
    #[serde(default)]
    pub plugins_manifest_digest: Option<String>,
}

fn default_language() -> String {
    "en".to_owned()
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
            plugins_manifest_digest: None,
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
            assert!(!result.ends_with(':'), "must not stay drive-relative: {result}");
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