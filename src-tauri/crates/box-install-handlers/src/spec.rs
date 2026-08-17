//! Spec syntax — the canonical, machine-readable form every install
//! handler consumes.
//!
//! The spec parser accepts both `dshbox`'s own `git:` / `npm:` prefix
//! forms and the npm/pnpm-aligned forms (`github:`, `npm:` alias,
//! `workspace:*`, `file:` / `link:`). A single `InstallSpec` value
//! drives every install handler so the boxfile parser, the `dshbox`
//! CLI, and the harness's internal `dsh plugin` layer can all agree on
//! what was asked for.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The kind of git host for `github:` / `bitbucket:` / `gitlab:` /
/// `git+https://` / `git+ssh://` short forms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitHost {
    Github,
    Bitbucket,
    Gitlab,
    GenericHttps,
    GenericSsh,
}

/// Workspace protocol flavor (pnpm 9+).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceProtocol {
    /// `workspace:*` — any version the workspace publishes.
    Any,
    /// `workspace:^` — carets-and-tildes version range from manifest.
    Caret,
    /// `workspace:~` — tilde range from manifest.
    Tilde,
    /// `workspace:^1.2.3` — explicit semver range.
    Range(String),
}

/// How a local filesystem source should be materialised inside the
/// container's profile `node_modules/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathMode {
    /// `pnpm add ./path` semantics — copy into a versioned store.
    Copy,
    /// `pnpm add link:../path` semantics — symlink into node_modules.
    Link,
}

/// Canonical install spec — one value drives every handler. Variants
/// match the 4 categories accepted by pnpm/DSH official plus dshbox's
/// own prefixes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallSpec {
    /// `npm:<pkg>`, bare `<pkg>`, `@scope/<name>[@ver]`, or
    /// `npm:` alias renaming (`my-alias@npm:real-pkg@1.0.0` is
    /// represented as `Alias { alias: "my-alias", target: Registry(..) }`).
    Registry {
        scope: Option<String>,
        name: String,
        version: Option<String>,
    },
    /// Git source — covers `github:`, `gitlab:`, `bitbucket:`, full
    /// `git+https://` / `git+ssh://` URLs, and dshbox's own `git:`
    /// prefix.
    Git {
        host: GitHost,
        url: String,
        ref_: Option<String>,
    },
    /// Local directory (with optional `link:` semantics).
    LocalPath { path: PathBuf, mode: PathMode },
    /// Local `.tar` / `.tar.gz` / `.tgz` file.
    LocalTarball { path: PathBuf },
    /// Remote tarball URL (https/http).
    RemoteTarball { url: String },
    /// `workspace:*` / `workspace:^` / `workspace:~` / `workspace:^X.Y.Z`.
    Workspace {
        name: String,
        protocol: WorkspaceProtocol,
    },
    /// `my-alias@<target>` — rename an arbitrary other spec.
    Alias {
        alias: String,
        target: Box<InstallSpec>,
    },
    /// `bun@runtime:1.3.0` — runtime specifier (pnpm 12+). Reserved
    /// for future implementation; the parser accepts the form but the
    /// handler returns a not-yet-implemented error today.
    Runtime { name: String, version: String },
}

impl InstallSpec {
    /// Stable human-readable label for logs.
    pub fn label(&self) -> String {
        match self {
            InstallSpec::Registry { scope, name, version } => match scope {
                Some(scope) => match version {
                    Some(v) => format!("@{scope}/{name}@{v}"),
                    None => format!("@{scope}/{name}"),
                },
                None => match version {
                    Some(v) => format!("{name}@{v}"),
                    None => name.clone(),
                },
            },
            InstallSpec::Git { url, ref_, .. } => match ref_ {
                Some(r) => format!("{url}#{r}"),
                None => url.clone(),
            },
            InstallSpec::LocalPath { path, mode } => {
                let prefix = match mode {
                    PathMode::Copy => "",
                    PathMode::Link => "link:",
                };
                format!("{prefix}{}", path.display())
            }
            InstallSpec::LocalTarball { path } => format!("file:{}", path.display()),
            InstallSpec::RemoteTarball { url } => url.clone(),
            InstallSpec::Workspace { name, protocol } => {
                let proto = match protocol {
                    WorkspaceProtocol::Any => "*",
                    WorkspaceProtocol::Caret => "^",
                    WorkspaceProtocol::Tilde => "~",
                    WorkspaceProtocol::Range(r) => r.as_str(),
                };
                format!("{name}@workspace:{proto}")
            }
            InstallSpec::Alias { alias, target } => format!("{alias}@{}", target.label()),
            InstallSpec::Runtime { name, version } => format!("{name}@runtime:{version}"),
        }
    }
}

/// Parse errors returned by [`parse_spec`].
#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("empty spec")]
    Empty,
    #[error("invalid git spec `{0}`: {1}")]
    InvalidGit(String, String),
    #[error("invalid npm alias `{0}`: {1}")]
    InvalidAlias(String, String),
    #[error("invalid workspace protocol `{0}`: {1}")]
    InvalidWorkspace(String, String),
    #[error("invalid runtime spec `{0}`")]
    InvalidRuntime(String),
}

/// Parse an install spec from a single token (whitespace-trimmed).
/// `base_dir` is used to resolve relative local paths.
pub fn parse_spec(input: &str, base_dir: &Path) -> Result<InstallSpec, SpecError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(SpecError::Empty);
    }

    // Alias form: split on the FIRST `@` that is followed by a
    // recognised spec-starting prefix. We accept `my-alias@npm:real`,
    // `my-alias@github:owner/repo`, `my-alias@workspace:*`,
    // `my-alias@file:...`, `my-alias@link:...`, `my-alias@http(s):...`,
    // `my-alias@runtime:X`, and `my-alias@git+...`. A scoped registry
    // name (`@scope/name@ver`) is NOT an alias — only un-scoped
    // `name@<prefix>:<rest>`.
    if let Some((alias, rest)) = split_alias(trimmed) {
        if alias.starts_with('@') {
            // Scoped registry form — fall through to the registry
            // branch.
        } else if rest.starts_with("workspace:") {
            // `name@workspace:<protocol>` — workspace is a top-level
            // spec, not an alias target.
            let protocol = parse_workspace_protocol(&rest["workspace:".len()..])?;
            return Ok(InstallSpec::Workspace {
                name: alias.to_owned(),
                protocol,
            });
        } else {
            let target = parse_spec(rest, base_dir)?;
            return Ok(InstallSpec::Alias {
                alias: alias.to_owned(),
                target: Box::new(target),
            });
        }
    }

    // `name@workspace:<protocol>` first — workspace is not a target
    // spec that can be wrapped in an alias, so we handle it before the
    // alias branch.
    if let Some(rest) = trimmed.strip_prefix("git:") {
        // dshbox-style git: prefix: the remainder is a host/owner/repo
        // short-form (e.g. `git:github.com/owner/repo:latest`). We
        // split host from the owner/repo[:ref] portion and feed it
        // through the same parser the `github:` / `gitlab:` branches
        // use, so the two prefixes stay in lock-step.
        let (host_label, owner_repo) = rest
            .split_once('/')
            .ok_or_else(|| SpecError::InvalidGit(rest.to_owned(), "git: needs host/owner/repo".to_owned()))?;
        let host = match host_label {
            "github.com" => GitHost::Github,
            "gitlab.com" => GitHost::Gitlab,
            "bitbucket.org" => GitHost::Bitbucket,
            other => {
                return Err(SpecError::InvalidGit(
                    format!("git:{other}/..."),
                    format!("git: only recognises github.com/gitlab.com/bitbucket.org hosts (got `{other}`)"),
                ));
            }
        };
        return parse_hosted_short(host, owner_repo);
    }
    if let Some(rest) = trimmed.strip_prefix("github:") {
        return parse_github_short(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("gitlab:") {
        return parse_hosted_short(GitHost::Gitlab, rest);
    }
    if let Some(rest) = trimmed.strip_prefix("bitbucket:") {
        return parse_hosted_short(GitHost::Bitbucket, rest);
    }
    if let Some(rest) = trimmed.strip_prefix("git+") {
        return parse_git_full(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("file:") {
        return parse_file_or_link(rest, PathMode::Copy, base_dir);
    }
    if let Some(rest) = trimmed.strip_prefix("link:") {
        return parse_file_or_link(rest, PathMode::Link, base_dir);
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        if looks_like_tarball(trimmed) {
            return Ok(InstallSpec::RemoteTarball {
                url: trimmed.to_owned(),
            });
        }
        // Could also be a git+https URL we missed; treat as remote tarball.
        return Ok(InstallSpec::RemoteTarball {
            url: trimmed.to_owned(),
        });
    }
    if let Some(rest) = trimmed.strip_prefix("npm:") {
        // npm: prefix. Could be a registry spec OR a rename alias
        // (e.g. `npm:yarn@1.22.22`). We try to split on `@version` /
        // `:tag`; if no version is present, treat as bare registry name.
        return parse_registry(rest, base_dir);
    }
    if trimmed.starts_with("runtime:") {
        return Err(SpecError::InvalidRuntime(trimmed.to_owned()));
    }

    // Bare path or bare registry name. Try path first; fall back to registry.
    if trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || trimmed.ends_with(".tar")
        || trimmed.ends_with(".tar.gz")
        || trimmed.ends_with(".tgz")
        || trimmed.ends_with(".tar.xz")
        || trimmed.ends_with(".txz")
    {
        return parse_local(trimmed, base_dir);
    }

    parse_registry(trimmed, base_dir)
}

fn split_alias(input: &str) -> Option<(&str, &str)> {
    // Find a `@` that has a recognised prefix after it. Skip the
    // leading `@` of a scoped name.
    let bytes = input.as_bytes();
    let mut start = 0;
    if bytes.first() == Some(&b'@') {
        // Could be `@scope/...`; advance to the next `/` + 1.
        if let Some(slash) = input[1..].find('/') {
            start = slash + 2; // position right after `@scope/`
        }
    }
    let search = &input[start..];
    for (offset, ch) in search.char_indices() {
        if ch == '@' {
            let rest = &search[offset + 1..];
            if rest.starts_with("npm:")
                || rest.starts_with("github:")
                || rest.starts_with("gitlab:")
                || rest.starts_with("bitbucket:")
                || rest.starts_with("workspace:")
                || rest.starts_with("file:")
                || rest.starts_with("link:")
                || rest.starts_with("git+")
                || rest.starts_with("runtime:")
            {
                let alias = &input[..start + offset];
                return Some((alias, rest));
            }
        }
    }
    None
}

fn parse_github_short(rest: &str) -> Result<InstallSpec, SpecError> {
    parse_hosted_short(GitHost::Github, rest)
}

fn parse_hosted_short(host: GitHost, rest: &str) -> Result<InstallSpec, SpecError> {
    // `owner/repo[#ref]` or `owner/repo[:tag]`.
    let (head, ref_) = split_ref(rest);
    let parts: Vec<&str> = head.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(SpecError::InvalidGit(
            format!("{:?}:{head}", host),
            "expected `owner/repo[#ref]`".to_owned(),
        ));
    }
    let owner = parts[0];
    let repo = parts[1].trim_end_matches(".git");
    let url = match &host {
        GitHost::Github => format!("https://github.com/{owner}/{repo}"),
        GitHost::Gitlab => format!("https://gitlab.com/{owner}/{repo}"),
        GitHost::Bitbucket => format!("https://bitbucket.org/{owner}/{repo}"),
        GitHost::GenericHttps | GitHost::GenericSsh => {
            return Err(SpecError::InvalidGit(
                format!("{:?}:{head}", host),
                "hosted-short parser called for generic host".to_owned(),
            ));
        }
    };
    Ok(InstallSpec::Git {
        host,
        url,
        ref_,
    })
}

fn parse_git_full(rest: &str) -> Result<InstallSpec, SpecError> {
    // `https://host/path[.git]` or `ssh://git@host/path[.git]`.
    let (head, ref_) = split_ref(rest);
    if !(head.starts_with("https://") || head.starts_with("http://") || head.starts_with("ssh://")) {
        return Err(SpecError::InvalidGit(
            format!("git+{head}"),
            "git+ URL must start with https/http/ssh".to_owned(),
        ));
    }
    let host = if head.starts_with("ssh://") {
        GitHost::GenericSsh
    } else {
        GitHost::GenericHttps
    };
    Ok(InstallSpec::Git {
        host,
        url: head.to_owned(),
        ref_,
    })
}

fn split_ref(token: &str) -> (String, Option<String>) {
    // `#ref` is the npm convention; `:` is also accepted (mirrors our
    // existing boxfile parser behaviour for GitHub short forms).
    if let Some((head, tail)) = token.split_once('#') {
        if !tail.is_empty() {
            return (head.to_owned(), Some(tail.to_owned()));
        }
    }
    // `:` — but only when the head looks like a URL / repo (no scheme
    // bytes before it) AND the tail is non-empty.
    if let Some((head, tail)) = token.split_once(':') {
        // Avoid splitting `https://`; only consider `:` after the scheme.
        let head_starts_with_scheme = head.contains("://");
        if !head_starts_with_scheme && !tail.is_empty() && !tail.contains('/') {
            return (head.to_owned(), Some(tail.to_owned()));
        }
        if head_starts_with_scheme {
            // Strip the scheme, look for a path-component `:tag`:
            //   https://host/owner/repo:tag   ← rare but allowed
            if let Some((path, tag)) = head.split_once(':') {
                if !tag.is_empty() {
                    return (format!("{path}{tag}"), None); // doesn't help
                }
            }
        }
    }
    (token.to_owned(), None)
}

fn parse_file_or_link(rest: &str, mode: PathMode, base_dir: &Path) -> Result<InstallSpec, SpecError> {
    if rest.starts_with("http://") || rest.starts_with("https://") {
        return Ok(InstallSpec::RemoteTarball {
            url: rest.to_owned(),
        });
    }
    let path = if rest.starts_with('/') {
        PathBuf::from(rest)
    } else {
        base_dir.join(rest)
    };
    if looks_like_tarball_str(rest) {
        return Ok(InstallSpec::LocalTarball { path });
    }
    Ok(InstallSpec::LocalPath { path, mode })
}

fn parse_local(rest: &str, base_dir: &Path) -> Result<InstallSpec, SpecError> {
    let path = if rest.starts_with('/') {
        PathBuf::from(rest)
    } else {
        base_dir.join(rest)
    };
    if looks_like_tarball_str(rest) {
        return Ok(InstallSpec::LocalTarball { path });
    }
    Ok(InstallSpec::LocalPath {
        path,
        mode: PathMode::Copy,
    })
}

fn parse_registry(input: &str, _base_dir: &Path) -> Result<InstallSpec, SpecError> {
    // `@scope/name@version` / `@scope/name:tag` / `name@version` /
    // `name:tag` / bare `name`.
    //
    // The tricky case is the scoped form: the leading `@` is part of
    // the scope, NOT a version separator. We split on `@` AFTER
    // peeling any `@scope/` prefix.
    let (scope, after_scope) = if let Some(rest) = input.strip_prefix('@') {
        if let Some((scope, after)) = rest.split_once('/') {
            (Some(scope.to_owned()), after)
        } else {
            // `@foo` with no slash — treat as malformed scoped name.
            return Err(SpecError::InvalidAlias(
                input.to_owned(),
                "scoped registry name must include `/name`".to_owned(),
            ));
        }
    } else {
        (None, input)
    };
    // Now `after_scope` is either `name@ver`, `name:tag`, or `name`.
    let (name, version) = split_name_version(after_scope)
        .ok_or_else(|| SpecError::InvalidAlias(input.to_owned(), "registry name is empty".to_owned()))?;
    if name.is_empty() {
        return Err(SpecError::InvalidAlias(
            input.to_owned(),
            "registry name is empty".to_owned(),
        ));
    }
    Ok(InstallSpec::Registry {
        scope,
        name: name.to_owned(),
        version,
    })
}

/// Split a (non-scoped) name+version tail. Returns `None` when the
/// input is empty. Accepts `@version` or `:tag` separators; the tail
/// must look like a version string (alphanum + `.-_+`).
fn split_name_version(input: &str) -> Option<(&str, Option<String>)> {
    if input.is_empty() {
        return None;
    }
    // Prefer `@` because pnpm/registry convention uses `@<exact>` and
    // `@<tag>`; fall back to `:` when no `@` is present.
    if let Some((name, tail)) = input.split_once('@') {
        if !name.is_empty() && !tail.is_empty() && !tail.contains('@') {
            return Some((name, Some(tail.to_owned())));
        }
        return Some((name, None));
    }
    if let Some((name, tail)) = input.split_once(':') {
        if !name.is_empty() && !tail.is_empty() && looks_like_version(tail) {
            return Some((name, Some(tail.to_owned())));
        }
        return Some((name, None));
    }
    Some((input, None))
}

fn looks_like_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
}

fn parse_workspace_protocol(input: &str) -> Result<WorkspaceProtocol, SpecError> {
    match input {
        "*" => Ok(WorkspaceProtocol::Any),
        "^" => Ok(WorkspaceProtocol::Caret),
        "~" => Ok(WorkspaceProtocol::Tilde),
        other => {
            if other.starts_with('^') || other.starts_with('~') || looks_like_version(other) {
                Ok(WorkspaceProtocol::Range(other.to_owned()))
            } else {
                Err(SpecError::InvalidWorkspace(
                    format!("workspace:{other}"),
                    "expected *, ^, ~, ^<range>, or <range>".to_owned(),
                ))
            }
        }
    }
}

fn looks_like_tarball(input: &str) -> bool {
    looks_like_tarball_str(input)
}

fn looks_like_tarball_str(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    lower.ends_with(".tar")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b() -> PathBuf {
        PathBuf::from("/tmp")
    }

    #[test]
    fn bare_name_parses_as_registry() {
        let s = parse_spec("lodash", &b()).unwrap();
        assert!(matches!(s, InstallSpec::Registry { name, .. } if name == "lodash"));
    }

    #[test]
    fn scoped_registry_with_at_version() {
        let s = parse_spec("@scope/name@1.2.3", &b()).unwrap();
        assert_eq!(
            s,
            InstallSpec::Registry {
                scope: Some("scope".to_owned()),
                name: "name".to_owned(),
                version: Some("1.2.3".to_owned()),
            }
        );
    }

    #[test]
    fn scoped_registry_with_colon_tag() {
        let s = parse_spec("@scope/name:beta", &b()).unwrap();
        assert_eq!(
            s,
            InstallSpec::Registry {
                scope: Some("scope".to_owned()),
                name: "name".to_owned(),
                version: Some("beta".to_owned()),
            }
        );
    }

    #[test]
    fn github_short_with_hash_ref() {
        let s = parse_spec("github:owner/repo#v1.0", &b()).unwrap();
        assert_eq!(
            s,
            InstallSpec::Git {
                host: GitHost::Github,
                url: "https://github.com/owner/repo".to_owned(),
                ref_: Some("v1.0".to_owned()),
            }
        );
    }

    #[test]
    fn dshbox_git_prefix_aliases_github_short() {
        let s = parse_spec("git:github.com/owner/repo:latest", &b()).unwrap();
        assert!(matches!(s, InstallSpec::Git { ref_: Some(t), .. } if t == "latest"));
    }

    #[test]
    fn npm_prefix_is_registry() {
        let s = parse_spec("npm:@scope/name@1.2.3", &b()).unwrap();
        assert_eq!(
            s,
            InstallSpec::Registry {
                scope: Some("scope".to_owned()),
                name: "name".to_owned(),
                version: Some("1.2.3".to_owned()),
            }
        );
    }

    #[test]
    fn npm_alias_renames_a_package() {
        let s = parse_spec("yarn@npm:yarn@1.22.22", &b()).unwrap();
        match s {
            InstallSpec::Alias { alias, target } => {
                assert_eq!(alias, "yarn");
                assert!(matches!(*target, InstallSpec::Registry { .. }));
            }
            _ => panic!("expected alias"),
        }
    }

    #[test]
    fn git_plus_https_full_url() {
        let s = parse_spec("git+https://example.com/team/repo.git", &b()).unwrap();
        assert!(matches!(s, InstallSpec::Git { host: GitHost::GenericHttps, .. }));
    }

    #[test]
    fn workspace_requires_at_prefix() {
        let s = parse_spec("my-pkg@workspace:*", &b()).unwrap();
        assert!(matches!(
            s,
            InstallSpec::Workspace {
                protocol: WorkspaceProtocol::Any,
                ..
            }
        ));
    }

    #[test]
    fn local_directory_with_copy_mode() {
        let s = parse_spec("./plugins/my-plugin", &b()).unwrap();
        match s {
            InstallSpec::LocalPath { mode, .. } => assert_eq!(mode, PathMode::Copy),
            _ => panic!("expected LocalPath"),
        }
    }

    #[test]
    fn link_prefix_uses_link_mode() {
        let s = parse_spec("link:../local", &b()).unwrap();
        match s {
            InstallSpec::LocalPath { mode, .. } => assert_eq!(mode, PathMode::Link),
            _ => panic!("expected LocalPath"),
        }
    }

    #[test]
    fn remote_tarball() {
        let s = parse_spec("https://example.com/foo.tar.gz", &b()).unwrap();
        assert!(matches!(s, InstallSpec::RemoteTarball { .. }));
    }

    #[test]
    fn local_tarball_extension_detection() {
        let s = parse_spec("./packs/audio.tar.gz", &b()).unwrap();
        assert!(matches!(s, InstallSpec::LocalTarball { .. }));
    }
}