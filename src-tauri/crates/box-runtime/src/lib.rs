//! Controlled execution primitives for feature crates. Tauri is intentionally absent.

use box_foundation::{suppress_console_window, BoxResult};
use git2::{build::RepoBuilder, FetchOptions, RemoteCallbacks};
use std::{
    path::Path,
    process::{Command, Output},
};

pub trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        executable: &Path,
        arguments: &[String],
        working_directory: Option<&Path>,
    ) -> BoxResult<Output>;
}

#[derive(Default)]
pub struct NativeProcessRunner;
impl ProcessRunner for NativeProcessRunner {
    fn run(
        &self,
        executable: &Path,
        arguments: &[String],
        working_directory: Option<&Path>,
    ) -> BoxResult<Output> {
        let mut command = Command::new(executable);
        suppress_console_window(&mut command);
        command.args(arguments);
        if let Some(directory) = working_directory {
            command.current_dir(directory);
        }
        command
            .output()
            .map_err(|error| format!("cannot run {}: {error}", executable.display()))
    }
}

/// Clone one public revision without requiring a Git executable on PATH.
pub fn shallow_clone(url: &str, destination: &Path, revision: Option<&str>) -> BoxResult<String> {
    shallow_clone_with_cancel(url, destination, revision, || false)
}

/// Best-effort detection of the user's HTTP proxy for git transfers.
/// libgit2 ignores Windows system proxy settings, so without this a clone of
/// github.com just times out whenever the machine only reaches the internet
/// through a local proxy (common in mainland China). Standard environment
/// variables win; otherwise the WinINET per-user proxy is read.
pub fn detect_proxy_url() -> Option<String> {
    for key in [
        "https_proxy",
        "HTTPS_PROXY",
        "http_proxy",
        "HTTP_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_owned();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let settings = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings")
            .ok()?;
        let enabled: u32 = settings.get_value("ProxyEnable").ok()?;
        if enabled == 0 {
            return None;
        }
        let server: String = settings.get_value("ProxyServer").ok()?;
        // ProxyServer is either "host:port" or a per-protocol list like
        // "http=host:port;https=host:port"; prefer the https entry.
        let mut plain = None;
        for part in server.split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some(rest) = part.strip_prefix("https=") {
                return Some(with_http_scheme(rest));
            }
            if let Some(rest) = part.strip_prefix("http=") {
                plain = Some(with_http_scheme(rest));
            }
            if !part.contains('=') && plain.is_none() {
                plain = Some(with_http_scheme(part));
            }
        }
        plain
    }
    #[cfg(not(target_os = "windows"))]
    None
}

// Windows-only helper: ProxyServer values from the registry often carry no
// scheme; normalize them before handing them to the HTTP client.
#[allow(dead_code)]
fn with_http_scheme(server: &str) -> String {
    let server = server.trim();
    if server.starts_with("http://")
        || server.starts_with("https://")
        || server.starts_with("socks5://")
    {
        server.to_owned()
    } else {
        format!("http://{server}")
    }
}

fn clone_once(
    url: &str,
    destination: &Path,
    revision: Option<&str>,
    cancelled: std::sync::Arc<std::sync::Mutex<dyn Fn() -> bool + Send>>,
    proxy_url: Option<&str>,
) -> BoxResult<String> {
    let mut fetch = FetchOptions::new();
    // libgit2's local transport does not implement shallow fetches. Local
    // repositories are useful for tests and development; public remotes stay shallow.
    if !Path::new(url).is_dir() {
        fetch.depth(1);
        // Shallow fetches only pull the default branch's tip commit; without
        // this call `revparse_single("v0.12.2")` cannot resolve tags
        // because the `refs/tags/v0.12.2` pointer is never downloaded.
        fetch.download_tags(git2::AutotagOption::All);
    }
    let mut callbacks = RemoteCallbacks::new();
    let cancelled_for_progress = std::sync::Arc::clone(&cancelled);
    callbacks.transfer_progress(move |_| {
        // A poisoned lock means the owning thread panicked; keep going.
        cancelled_for_progress
            .lock()
            .map(|guard| !guard())
            .unwrap_or(true)
    });
    fetch.remote_callbacks(callbacks);
    if !Path::new(url).is_dir() {
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
    }
    let mut builder = RepoBuilder::new();
    builder.fetch_options(fetch);
    let repository = builder
        .clone(url, destination)
        .map_err(|error| format!("Git clone failed: {error}"))?;
    // Resolve the requested revision after clone. RepoBuilder::branch only
    // matches refs/heads, so it silently misses tags and trips with
    // `reference 'refs/remotes/origin/<ref>' not found`. revparse_single
    // walks the usual tag → branch → commit resolution order instead.
    let commit = if let Some(revision) = revision {
        let object = repository
            .revparse_single(revision)
            .map_err(|error| format!("cannot resolve revision `{revision}`: {error}"))?;
        repository
            .checkout_tree(&object, None)
            .map_err(|error| format!("cannot checkout `{revision}`: {error}"))?;
        repository
            .set_head_detached(object.id())
            .map_err(|error| format!("cannot detach HEAD to `{revision}`: {error}"))?;
        object
            .peel_to_commit()
            .map_err(|error| format!("cannot resolve cloned commit: {error}"))?
    } else {
        repository
            .head()
            .map_err(|error| format!("cannot resolve cloned revision: {error}"))?
            .peel_to_commit()
            .map_err(|error| format!("cannot resolve cloned commit: {error}"))?
    };
    Ok(commit.id().to_string())
}

/// Clone a public revision and stop the libgit2 transfer when cancellation is requested.
/// Local paths never go through a proxy. Remote clones try the detected system
/// proxy first and fall back to a direct connection, so machines with or
/// without a proxy both work without configuration.
pub fn shallow_clone_with_cancel(
    url: &str,
    destination: &Path,
    revision: Option<&str>,
    cancelled: impl Fn() -> bool + Send + 'static,
) -> BoxResult<String> {
    // Mutex<dyn Fn> is Sync as long as the closure is Send, so callers do not
    // have to hand us a Sync closure; the guard is only ever held briefly.
    let cancelled: std::sync::Arc<std::sync::Mutex<dyn Fn() -> bool + Send>> =
        std::sync::Arc::new(std::sync::Mutex::new(cancelled));
    if Path::new(url).is_dir() {
        return clone_once(url, destination, revision, cancelled, None);
    }
    let proxy = detect_proxy_url();
    if let Some(proxy_url) = proxy.as_deref() {
        match clone_once(url, destination, revision, std::sync::Arc::clone(&cancelled), Some(proxy_url)) {
            Ok(commit) => return Ok(commit),
            Err(error) => {
                // The proxy may be stale or blocking git; retry directly
                // (Option::None falls back to libgit2's own detection, which
                // is a plain connection when no proxy env vars are set).
                let _ = std::fs::remove_dir_all(destination);
                if let Ok(commit) =
                    clone_once(url, destination, revision, std::sync::Arc::clone(&cancelled), None)
                {
                    return Ok(commit);
                }
                return Err(error);
            }
        }
    }
    clone_once(url, destination, revision, cancelled, None)
}

/// Best-effort cleanup after a failed checkout.
pub fn remove_checkout(destination: &Path) {
    if destination.exists() {
        let _ = std::fs::remove_dir_all(destination);
    }
}

/// List the tags a public remote advertises, without downloading any
/// objects — libgit2's equivalent of `git ls-remote --tags`, keeping the
/// project free of any dependency on a system git executable. Like the
/// clone path, remote listings try the detected system proxy first and
/// fall back to a direct connection; a local directory is read from its
/// `refs/tags/` namespace directly.
pub fn list_remote_tags(url: &str) -> BoxResult<Vec<String>> {
    // Local repositories (tests / development): read tags off disk instead
    // of negotiating a smart-protocol connection with ourselves.
    if Path::new(url).is_dir() {
        let repository = git2::Repository::open(url)
            .map_err(|error| format!("cannot open local repository: {error}"))?;
        let mut tags = Vec::new();
        for reference in repository
            .references()
            .map_err(|error| format!("cannot list local refs: {error}"))?
            .flatten()
        {
            if let Some(tag) = reference.name().and_then(|name| name.strip_prefix("refs/tags/")) {
                tags.push(tag.to_owned());
            }
        }
        tags.sort();
        return Ok(tags);
    }
    let proxy = detect_proxy_url();
    if let Some(proxy_url) = proxy.as_deref() {
        match list_remote_tags_once(url, Some(proxy_url)) {
            Ok(tags) => return Ok(tags),
            Err(error) => {
                // The proxy may be stale or blocking git; retry directly.
                if let Ok(tags) = list_remote_tags_once(url, None) {
                    return Ok(tags);
                }
                return Err(error);
            }
        }
    }
    list_remote_tags_once(url, None)
}

fn list_remote_tags_once(url: &str, proxy_url: Option<&str>) -> BoxResult<Vec<String>> {
    // An anonymous remote on a throwaway bare repository is enough to run
    // the ref advertisement; nothing is persisted or fetched.
    let scratch = std::env::temp_dir().join(format!(
        "dsh-box-lsremote-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let repository = git2::Repository::init_bare(&scratch)
        .map_err(|error| format!("cannot prepare git session: {error}"))?;
    let result = (|| -> BoxResult<Vec<String>> {
        let mut remote = repository
            .remote_anonymous(url)
            .map_err(|error| format!("cannot resolve remote {url}: {error}"))?;
        let mut proxy = git2::ProxyOptions::new();
        match proxy_url {
            Some(proxy_url) => {
                proxy.url(proxy_url);
            }
            None => {
                proxy.auto();
            }
        }
        let connection = remote
            .connect_auth(git2::Direction::Fetch, None, Some(proxy))
            .map_err(|error| format!("cannot reach {url}: {error}"))?;
        let mut tags = Vec::new();
        for head in connection
            .list()
            .map_err(|error| format!("cannot list refs of {url}: {error}"))?
        {
            let name = head.name();
            // Skip peeled tag objects (`refs/tags/v1^{}`); the plain ref
            // already names the tag.
            if let Some(tag) = name.strip_prefix("refs/tags/") {
                if !tag.ends_with("^{}") {
                    tags.push(tag.to_owned());
                }
            }
        }
        tags.sort();
        Ok(tags)
    })();
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn shallow_clone_uses_libgit2_not_a_git_executable() {
        let root = std::env::temp_dir().join(format!(
            "dsh-box-git-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source).unwrap();
        let repository = Repository::init(&source).unwrap();
        fs::write(source.join("README.md"), "source").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        let commit = shallow_clone(source.to_str().unwrap(), &destination, None).unwrap();
        assert!(destination.join("README.md").is_file());
        assert_eq!(commit.len(), 40);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_remote_tags_reads_local_repository_tags() {
        let root = std::env::temp_dir().join(format!(
            "dsh-box-lstags-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Repository::init(&root).unwrap();
        fs::write(root.join("README.md"), "tagged").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        let commit_id = repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        let commit = repository.find_commit(commit_id).unwrap();
        repository.tag("v0.1.0", commit.as_object(), &signature, "release", false).unwrap();
        repository.tag("v0.2.0", commit.as_object(), &signature, "release", false).unwrap();
        let tags = list_remote_tags(root.to_str().unwrap()).unwrap();
        assert_eq!(tags, vec!["v0.1.0".to_owned(), "v0.2.0".to_owned()]);
        let _ = fs::remove_dir_all(root);
    }
}
