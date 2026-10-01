//! Headless-browser discovery and launch for the DSH Box debug surface.
//!
//! DSH Box drives a Chromium-family browser over the DevTools Protocol to
//! expose page debugging (screenshot, element query, clicks) to external
//! callers. It never bundles an engine: a headless-capable Chromium is
//! already present on the machines Box runs on, and bundling one would add
//! ~200 MB of payload for a feature most installs never touch.
//!
//! Resolution order:
//!
//! 1. An explicit override — `BoxConfig::browser_path`, or the path an
//!    agent hands to the `debug_set_browser_path` RPC. Wins outright, so a
//!    portable or custom Chromium is always reachable.
//! 2. The well-known install locations, probed in a fixed order.
//!
//! A discovered path is only a candidate: it is not trusted until it is
//! actually launched, and every method that takes a caller-supplied path
//! validates it first (see `validate_browser_path`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod session;

pub use session::BrowserSession;

/// Which Chromium-family browser a candidate is, so diagnostics can say
/// something more useful than "a browser".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserKind {
    /// Microsoft Edge — preinstalled on Windows 10/11.
    Edge,
    /// Google Chrome.
    Chrome,
    /// Brave, built on Chromium and therefore CDP-compatible.
    Brave,
    /// Any other Chromium build.
    Chromium,
}

impl BrowserKind {
    /// Stable lowercase name for logs and RPC replies.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Chrome => "chrome",
            Self::Brave => "brave",
            Self::Chromium => "chromium",
        }
    }
}

/// A browser executable Box found, or was told to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCandidate {
    pub kind: BrowserKind,
    /// Absolute path to the executable.
    pub path: PathBuf,
    /// True when the path came from configuration rather than discovery.
    pub configured: bool,
}

/// Join a `/`-separated relative path onto a root one component at a
/// time.
///
/// `Path::join` keeps whatever separator the string already contains, so a
/// literal like `root.join("Microsoft/Edge/...")` yields a path that mixes
/// `\\` and `/`. It resolves correctly, but these paths are handed to agents
/// over RPC and pasted back into commands, so they are normalised here
/// instead of leaking mixed separators into every diagnostic.
fn sub(root: &Path, relative: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for part in relative.split('/').filter(|part| !part.is_empty()) {
        path.push(part);
    }
    path
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn well_known() -> Vec<(BrowserKind, PathBuf)> {
    let mut roots: Vec<PathBuf> = Vec::new();
    // Program Files first: a machine-wide install outranks a per-user one,
    // and the x86 root is checked before the 64-bit one because that is
    // where Edge installs itself on x64 Windows.
    if let Some(dir) = env_path("ProgramFiles(x86)") {
        roots.push(dir);
    }
    if let Some(dir) = env_path("ProgramFiles") {
        roots.push(dir);
    }
    if let Some(dir) = env_path("LOCALAPPDATA") {
        roots.push(dir);
    }

    let mut candidates = Vec::new();
    for root in &roots {
        candidates.push((
            BrowserKind::Edge,
            sub(root, "Microsoft/Edge/Application/msedge.exe"),
        ));
        candidates.push((
            BrowserKind::Chrome,
            sub(root, "Google/Chrome/Application/chrome.exe"),
        ));
        candidates.push((
            BrowserKind::Brave,
            sub(root, "BraveSoftware/Brave-Browser/Application/brave.exe"),
        ));
        candidates.push((
            BrowserKind::Chromium,
            sub(root, "Chromium/Application/chrome.exe"),
        ));
    }
    // Keep the per-root sweep but present a stable global preference order:
    // Edge (always present) before the rest, so the auto-detected answer is
    // the least surprising one on a stock Windows machine.
    let mut ordered = Vec::with_capacity(candidates.len());
    for kind in [
        BrowserKind::Edge,
        BrowserKind::Chrome,
        BrowserKind::Brave,
        BrowserKind::Chromium,
    ] {
        ordered.extend(candidates.iter().filter(|(k, _)| *k == kind).cloned());
    }
    ordered
}

#[cfg(not(windows))]
fn well_known() -> Vec<(BrowserKind, PathBuf)> {
    // Linux distributions and macOS both place Chromium builds in a handful
    // of conventional spots; PATH lookups are covered separately.
    vec![
        (BrowserKind::Edge, PathBuf::from("/usr/bin/microsoft-edge")),
        (
            BrowserKind::Edge,
            PathBuf::from("/usr/bin/microsoft-edge-stable"),
        ),
        (BrowserKind::Chrome, PathBuf::from("/usr/bin/google-chrome")),
        (BrowserKind::Chrome, PathBuf::from("/opt/google/chrome/chrome")),
        (BrowserKind::Chromium, PathBuf::from("/usr/bin/chromium")),
        (BrowserKind::Chromium, PathBuf::from("/usr/bin/chromium-browser")),
        (
            BrowserKind::Chrome,
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        ),
        (
            BrowserKind::Edge,
            PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
        ),
        (
            BrowserKind::Chromium,
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ),
    ]
}

/// Classify an arbitrary executable by its file name.
///
/// A configured path may point at a Chromium build Box has never seen, so
/// the kind is inferred rather than rejected.
pub fn classify(path: &Path) -> BrowserKind {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.contains("edge") {
        BrowserKind::Edge
    } else if name.contains("brave") {
        BrowserKind::Brave
    } else if name.contains("chrome") {
        BrowserKind::Chrome
    } else {
        BrowserKind::Chromium
    }
}

/// Probe the well-known install locations, returning the ones that exist.
///
/// Order is the resolution order documented on the module, so the first
/// entry is what auto-detection would choose.
pub fn discover() -> Vec<BrowserCandidate> {
    well_known()
        .into_iter()
        .filter(|(_, path)| path.is_file())
        .map(|(kind, path)| BrowserCandidate {
            kind,
            path,
            configured: false,
        })
        .collect()
}

/// Validate a caller-supplied browser path before it is ever executed.
///
/// A path arriving from the agent becomes a process launch, so it is held to
/// the same bar as any other executable Box runs: it must exist, be a regular
/// file, and be absolute. Relative paths are refused rather than resolved
/// against the daemon's working directory, which is not a meaningful base
/// for the caller.
pub fn validate_browser_path(path: &str) -> Result<PathBuf, String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("browser path is empty".to_owned());
    }
    let candidate = PathBuf::from(trimmed);
    if !candidate.is_absolute() {
        return Err(format!(
            "browser path must be absolute, got {trimmed}"
        ));
    }
    if !candidate.is_file() {
        return Err(format!("no browser executable at {trimmed}"));
    }
    Ok(candidate)
}

/// Resolve the browser to drive.
///
/// An explicit path wins over discovery, and a configured path that does not
/// exist is an error rather than a silent fallback: the user asked for that
/// browser, and quietly using a different one would make a debugging session
/// describe the wrong engine.
pub fn resolve(configured: Option<&str>) -> Result<BrowserCandidate, String> {
    if let Some(path) = configured.map(str::trim).filter(|value| !value.is_empty()) {
        let path = validate_browser_path(path)?;
        return Ok(BrowserCandidate {
            kind: classify(&path),
            path,
            configured: true,
        });
    }

    let found = discover();
    found
        .into_iter()
        .next()
        .ok_or_else(|| no_browser_message())
}

fn no_browser_message() -> String {
    [
        "no Chromium-family browser found for the debug surface.",
        "Install Microsoft Edge or Google Chrome, or point DSH Box at one with:",
        "  dshbox config set browser <absolute path to executable>",
    ]
    .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_file_name() {
        assert_eq!(
            classify(Path::new(r"C:Program Files (x86)MicrosoftEdgeApplicationmsedge.exe")),
            BrowserKind::Edge
        );
        assert_eq!(
            classify(Path::new("/usr/bin/google-chrome")),
            BrowserKind::Chrome
        );
        assert_eq!(
            classify(Path::new("/opt/portable/headless-shell")),
            BrowserKind::Chromium
        );
    }

    #[test]
    fn configured_path_wins_over_discovery() {
        // Use this very test binary as a stand-in: it is absolute and is a file.
        let exe = std::env::current_exe().expect("current exe");
        let resolved = resolve(Some(&exe.display().to_string())).expect("resolve");
        assert!(resolved.configured);
        assert_eq!(resolved.path, exe);
    }

    #[test]
    fn missing_configured_path_is_an_error_not_a_fallback() {
        let err = resolve(Some("C:/definitely/not/a/browser.exe")).unwrap_err();
        assert!(err.contains("no browser executable"), "unexpected: {err}");
    }

    #[test]
    fn empty_configured_path_falls_back_to_discovery() {
        // resolve() must not treat "" as a configured browser.
        let _ = resolve(Some(""));
    }

    #[test]
    fn relative_paths_are_refused() {
        let err = validate_browser_path("msedge.exe").unwrap_err();
        assert!(err.contains("must be absolute"), "unexpected: {err}");
    }

    #[test]
    fn discovery_is_ordered() {
        let found = discover();
        // Whatever exists, the order must match the documented preference.
        let order = |k: &BrowserKind| match k {
            BrowserKind::Edge => 0,
            BrowserKind::Chrome => 1,
            BrowserKind::Brave => 2,
            BrowserKind::Chromium => 3,
        };
        let ranks: Vec<u8> = found.iter().map(|c| order(&c.kind)).collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(ranks, sorted, "discovery order drifted: {found:?}");
        assert!(found.iter().all(|c| c.path.is_file()));
    }
}
