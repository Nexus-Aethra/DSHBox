//! Parser for `.dsh` build scripts. The grammar:
//!
//! ```text
//! FROM <harness-source>        # required, exactly once
//! PROFILE <name>               # required, exactly once
//! NAME <image-name>            # optional (default = script stem)
//! VERSION <image-version>      # optional (default = "latest")
//! LABEL key=value              # optional, repeatable
//! DEF <name> @<path>           # optional, repeatable (path alias)
//!
//! ADD plugin|skill <source> [@<dest>]   # one or more
//! CP <source> [@<dest>]                 # alias for `ADD plugin`
//! ```
//!
//! `<source>` follows four shapes:
//!
//! 1. GitHub short form: `github.com/owner/repo[:tag|@ref]`
//! 2. URL or local tarball: `https://...` / `./relative.tgz` / `/abs/path.tgz`
//! 3. Local directory: `./plugins/foo` / `/abs/path/foo` (no archive suffix)
//! 4. Bare package name (already imported into Repository): `name[@version]` or `@scope/name[@version]`
//!
//! The parser does not validate that sources actually exist; that's the
//! build step's job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ImageError;

/// Kind of an ADD operation. Retained as syntax sugar; the builder
/// dispatches on this to decide install hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddKind {
    Plugin,
    Skill,
    /// Non-code payload (datasets, media packs, config blobs). These are
    /// copied per-container instead of linked; they also carry no install
    /// hook, so `to_extension_kind` returns `None`.
    Data,
}

impl AddKind {
    /// Convert to the corresponding `ExtensionKind` for fetch/install
    /// operations. `Data` has no repository extension and returns `None`.
    pub fn to_extension_kind(self) -> Option<box_extensions::ExtensionKind> {
        match self {
            AddKind::Plugin => Some(box_extensions::ExtensionKind::Plugin),
            AddKind::Skill => Some(box_extensions::ExtensionKind::Skill),
            AddKind::Data => None,
        }
    }
}

/// Parsed build script. `ops` are kept in source order so error reporting
/// stays predictable.
///
/// `harness_url` + `harness_ref` carry the runtime source location. The
/// builder turns these into the first `ResolvedAdd` of the manifest.
/// `harness_digest()` produces a content-addressable ID for that slot.
///
/// `defs` holds user-defined path aliases (`DEF <name> @<path>`).
///
/// `base_template` is set when FROM references a local template name
/// (e.g. `FROM web-base`) instead of a harness GitHub reference. In that
/// mode `harness_url`/`harness_ref` are left empty; the builder resolves
/// the template chain and inherits the base's harness and profile, adding
/// only this template's own ops (incremental semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageScript {
    pub name: String,
    pub version: String,
    pub harness_url: String,
    pub harness_ref: Option<String>,
    pub base_template: Option<String>,
    pub profile: String,
    pub labels: BTreeMap<String, String>,
    pub defs: BTreeMap<String, String>,
    pub ops: Vec<ImageOp>,
}

impl ImageScript {
    /// Stable ID for the harness slot. Used as the digest for the
    /// content-addressable blob reference in the manifest.
    pub fn harness_digest(&self) -> String {
        let raw = match &self.harness_ref {
            Some(reference) => format!("{}@{reference}", self.harness_url),
            None => self.harness_url.clone(),
        };
        fnv1a64_hex(&raw)
    }
}

/// fnv1a 64-bit hash, hex-encoded. Same algorithm as `extension_digest` in
/// `box-extensions`, kept inline so the parser crate stays dependency-light.
fn fnv1a64_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// One operation. Today only `Add` exists; future ops (EDIT, EXEC, etc.)
/// hang off the same enum so the parser stays single-pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageOp {
    Add {
        kind: AddKind,
        source: ParsedSource,
        line: usize,
    },
}

/// Resolution of a single ADD source. The variants map 1:1 to the
/// shapes documented on the module-level rustdoc. `GitPrefix` and
/// `NpmPrefix` are explicit `git:` / `npm:` prefix forms that disambiguate
/// the source kind at parse time without guessing from token shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSource {
    /// GitHub clone, optionally pinned to a tag/branch/commit.
    Github { url: String, ref_: Option<String> },
    /// URL or local path to a tarball (`.tar`, `.tar.gz`, `.tgz`, `.tar.xz`).
    Tarball { url: String, local: bool },
    /// Local directory the builder will copy into Repository; the source
    /// path is resolved relative to the script directory at parse time.
    LocalDir { path: PathBuf },
    /// Bare package name (already in Repository). `scope` is the optional
    /// `@scope/` part; `version` is the optional `@1.2.3` / `:1.2.3` pin.
    BareName {
        name: String,
        scope: Option<String>,
        version: Option<String>,
    },
    /// Full pnpm spec syntax — `npm:@scope/name@1.2.3` (registry rename),
    /// `workspace:*`, `git+https://...`, `file:./path`, etc. The builder
    /// forwards this verbatim to `pnpm pack`; everything else still
    /// dispatches through the variants above.
    Passthrough { spec: String },
    /// Explicit `git:<ref>` form. The builder clones a GitHub repo just
    /// like the implicit `host/owner/repo[:tag|@ref]` shape, but the
    /// explicit prefix documents intent and stays unambiguous when the
    /// ref would otherwise look like a bare name.
    GitPrefix { ref_: String },
    /// Explicit `npm:<spec>` form. The builder resolves `<spec>` from
    /// the configured npm registry (`pnpm pack <spec>`), imports the
    /// resulting tarball into Repository, and runs the post-import
    /// dependency install.
    NpmPrefix { spec: String },
}

/// Tokenize and parse a script. `base_dir` is used to resolve relative
/// local sources; it does not need to exist for the parser itself.
pub fn parse_script(source: &str, base_dir: &Path) -> Result<ImageScript, ImageError> {
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut harness_url: Option<(usize, String)> = None;
    let mut harness_ref: Option<String> = None;
    let mut profile: Option<(usize, String)> = None;
    let mut base_template: Option<String> = None;
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    let mut defs: BTreeMap<String, String> = BTreeMap::new();
    let mut ops: Vec<ImageOp> = Vec::new();

    for (index, raw_line) in source.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = strip_comment(raw_line).trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let Some(first) = tokens.next() else {
            continue;
        };
        // `CP` is the legacy alias for `ADD plugin`. Collapse it before the
        // directive table so the rest of the parser stays shared.
        let keyword = if first == "CP" { "ADD" } else { first };
        let rest: Vec<&str> = tokens.collect();
        match keyword {
            "FROM" => {
                let value = expect_single_token(line_number, "FROM", &rest)?;
                if value.is_empty() {
                    return Err(ImageError::InvalidSource {
                        line: line_number,
                        source: trimmed.to_string(),
                        reason: "FROM value cannot be empty".to_string(),
                    });
                }
                if is_github_short(value) {
                    // Harness reference: `github.com/<owner>/<repo>[:tag|@ref]`.
                    let normalized = parse_from_value(value);
                    if let Some(reference) = &normalized.ref_ {
                        harness_ref = Some(reference.clone());
                    }
                    harness_url = Some((line_number, normalized.url));
                } else {
                    // Local template reference: the builder looks it up
                    // under `<root>/templates/<name>.dsh` and inherits the
                    // base's harness/profile, adding only this template's
                    // own ops.
                    base_template = Some(value.to_string());
                }
            }
            "PROFILE" => {
                let value = expect_single_token(line_number, "PROFILE", &rest)?;
                if value.is_empty() {
                    return Err(ImageError::InvalidSource {
                        line: line_number,
                        source: trimmed.to_string(),
                        reason: "PROFILE value cannot be empty".to_string(),
                    });
                }
                profile = Some((line_number, value.to_string()));
            }
            "NAME" => {
                let value = expect_single_token(line_number, "NAME", &rest)?;
                name = Some(value.to_string());
            }
            "VERSION" => {
                let value = expect_single_token(line_number, "VERSION", &rest)?;
                version = Some(value.to_string());
            }
            "LABEL" => {
                if rest.len() != 1 {
                    return Err(ImageError::Syntax {
                        line: line_number,
                        message: "LABEL expects exactly one key=value pair".to_string(),
                    });
                }
                let (key, value) = rest[0].split_once('=').ok_or_else(|| ImageError::Syntax {
                    line: line_number,
                    message: "LABEL value must use key=value syntax".to_string(),
                })?;
                labels.insert(key.trim().to_string(), value.trim().to_string());
            }
            "DEF" => {
                if rest.len() != 2 {
                    return Err(ImageError::Syntax {
                        line: line_number,
                        message: "DEF expects `DEF <name> @<path>`".to_string(),
                    });
                }
                let name = rest[0].to_string();
                let path = rest[1].to_string();
                if !path.starts_with('@') {
                    return Err(ImageError::Syntax {
                        line: line_number,
                        message: "DEF path must start with @".to_string(),
                    });
                }
                if defs.contains_key(&name) {
                    return Err(ImageError::Syntax {
                        line: line_number,
                        message: format!("DEF `{name}` is already defined"),
                    });
                }
                defs.insert(name, path);
            }
            "ADD" => {
                if rest.is_empty() || rest.len() > 3 {
                    return Err(ImageError::Syntax {
                        line: line_number,
                        message: "ADD expects `ADD plugin|skill <source> [@<dest>]`".to_string(),
                    });
                }
                // Resolve the kind token: explicit `plugin`/`skill` keeps its
                // kind; `data` marks a non-code payload (copied per-container,
                // never linked); any other leading token means a bare source
                // and is treated as `plugin` (so the legacy `CP <source>` form
                // keeps working).
                let (kind_token, source_index) = match rest[0] {
                    "plugin" | "skill" | "data" => (rest[0], 1),
                    _ => ("plugin", 0),
                };
                let source_token = rest.get(source_index).copied().ok_or_else(|| ImageError::Syntax {
                    line: line_number,
                    message: "ADD expects a source token".to_string(),
                })?;
                let kind = match kind_token {
                    "plugin" => AddKind::Plugin,
                    "skill" => AddKind::Skill,
                    "data" => AddKind::Data,
                    _ => unreachable!(),
                };
                let parsed = parse_source_token(line_number, source_token, base_dir)?;
                ops.push(ImageOp::Add {
                    kind,
                    source: parsed,
                    line: line_number,
                });
            }
            other => {
                return Err(ImageError::Syntax {
                    line: line_number,
                    message: format!("unknown directive `{other}`"),
                });
            }
        }
    }

    // FROM must appear exactly once. A GitHub short form carries the
    // harness; any other value names a local template whose chain the
    // builder resolves (harness fields are filled in by that pass).
    let (harness_url, harness_ref, base_template) = if base_template.is_none() {
        let (_, url) = harness_url
            .ok_or(ImageError::MissingDirective { line: 0, name: "FROM" })?;
        (url, harness_ref, None)
    } else {
        (String::new(), None, base_template)
    };
    let (_, profile) = profile.ok_or(ImageError::MissingDirective { line: 0, name: "PROFILE" })?;

    let resolved_name = name.unwrap_or_else(|| "image".to_string());
    let resolved_version = version.unwrap_or_else(|| "latest".to_string());

    Ok(ImageScript {
        name: resolved_name,
        version: resolved_version,
        harness_url,
        harness_ref,
        base_template,
        profile,
        labels,
        defs,
        ops,
    })
}

fn strip_comment(line: &str) -> &str {
    // "#" starts a line comment only when it is the first non-whitespace
    // character. We deliberately do not support inline `#` because most
    // sources (GitHub URLs, paths) may legitimately contain `#`.
    let trimmed_start = line.trim_start();
    if trimmed_start.starts_with('#') {
        return "";
    }
    line
}

fn expect_single_token<'a>(
    line_number: usize,
    keyword: &str,
    rest: &[&'a str],
) -> Result<&'a str, ImageError> {
    if rest.len() != 1 {
        return Err(ImageError::Syntax {
            line: line_number,
            message: format!("{keyword} expects exactly one argument"),
        });
    }
    Ok(rest[0])
}

/// Decide which `ParsedSource` variant a single ADD source token maps to.
/// Order is significant: `://` -> URL, `/` prefix -> absolute, `./` or `../`
/// -> relative, then GitHub short form, then bare name. This matches the
/// behaviour described in the plan.
pub fn parse_source_token(
    line: usize,
    token: &str,
    base_dir: &Path,
) -> Result<ParsedSource, ImageError> {
    if token.is_empty() {
        return Err(ImageError::InvalidSource {
            line,
            source: token.to_string(),
            reason: "source cannot be empty".to_string(),
        });
    }

    // Explicit `git:` / `npm:` prefix forms. The prefix is the only
    // authoritative signal of intent; the remainder of the token is
    // forwarded verbatim to the respective resolver. We refuse empty
    // payloads so a stray `ADD plugin npm:` produces a clear error
    // instead of falling through to the bare-name branch.
    if let Some(rest) = token.strip_prefix("npm:") {
        if rest.is_empty() {
            return Err(ImageError::InvalidSource {
                line,
                source: token.to_string(),
                reason: "npm: prefix requires a spec (e.g. `npm:@scope/name@1.2.3`)".to_string(),
            });
        }
        return Ok(ParsedSource::NpmPrefix { spec: rest.to_owned() });
    }
    if let Some(rest) = token.strip_prefix("git:") {
        if rest.is_empty() {
            return Err(ImageError::InvalidSource {
                line,
                source: token.to_string(),
                reason: "git: prefix requires a ref (e.g. `git:github.com/owner/repo:latest`)".to_string(),
            });
        }
        return Ok(ParsedSource::GitPrefix { ref_: rest.to_owned() });
    }

    if token.contains("://") {
        // `git+https://...` / `git+ssh://...` are git sources, not
        // tarballs. Forward them as passthrough specs so pnpm handles them
        // natively; no need to re-validate against a local parser.
        if token.starts_with("git+https://")
            || token.starts_with("git+http://")
            || token.starts_with("git+ssh://")
        {
            return Ok(ParsedSource::Passthrough {
                spec: token.to_owned(),
            });
        }
        let local = !(token.starts_with("http://") || token.starts_with("https://"));
        return Ok(ParsedSource::Tarball { url: token.to_string(), local });
    }

    if token.starts_with('/') {
        let path = PathBuf::from(token);
        return classify_path(line, token, path);
    }

    if token.starts_with("./") || token.starts_with("../") {
        let joined = base_dir.join(token);
        let canonical_hint = joined.to_string_lossy().into_owned();
        // We don't canonicalize at parse time because the source might not
        // exist yet; we just record the joined path and let the builder
        // decide.
        let stripped = canonical_hint
            .strip_prefix(base_dir.to_string_lossy().as_ref())
            .map(|value| value.to_string())
            .unwrap_or(canonical_hint);
        return classify_path(line, stripped.trim_start_matches('/'), joined);
    }

    if is_github_short(token) {
        let (url, ref_) = split_github_ref(token);
        return Ok(ParsedSource::Github { url, ref_ });
    }

    // Bare names are of the shape `[<scope>/]<package>[[@|:]<version>]`. We
    // intentionally do the scope split *before* the version split so that a
    // leading `@scope/` doesn't get mistaken for a version pin.
    let (scope_raw, after_scope) = split_scope_token(token);
    let (name, version) = split_at_ref(after_scope);
    if name.is_empty() {
        return Err(ImageError::InvalidSource {
            line,
            source: token.to_string(),
            reason: "bare name must include a package (use `name` or `@scope/name`)"
                .to_string(),
        });
    }

    // npm alias form `my-alias@npm:real-pkg@ver` — the `@` is part of
    // the npm alias grammar, not a version delimiter. Catch it before the
    // bare-name split misreads it as `BareName{name, version:"npm:..."}`.
    if token.contains("@npm:") {
        return Ok(ParsedSource::Passthrough {
            spec: token.to_owned(),
        });
    }

    // Last-resort passthrough: tokens starting with a pnpm-specific prefix
    // or containing `://` (git+https, git+ssh) are forwarded verbatim so
    // pnpm handles the full syntax. A bare `name[:version]` or
    // `@scope/name[:version]` stays as `BareName` so the existing
    // dedup-by-name semantics work.
    let starts_with_prefix = token.starts_with("npm:")
        || token.starts_with("workspace:")
        || token.starts_with("file:")
        || token.starts_with("link:")
        || token.starts_with("github:")
        || token.starts_with("gitlab:")
        || token.starts_with("bitbucket:");
    let has_git_scheme = token.contains("://");
    if starts_with_prefix || has_git_scheme {
        return Ok(ParsedSource::Passthrough {
            spec: token.to_owned(),
        });
    }
    Ok(ParsedSource::BareName {
        name: name.to_string(),
        scope: scope_raw,
        version,
    })
}

// Reduce a FROM value to the bare DSH version name. The canonical
// harness reference is github.com/<owner>/<repo>[:tag|@ref]; we keep the
// tag/ref and drop the host/owner/repo path so the builder can look the
struct NormalizedFrom {
    url: String,
    ref_: Option<String>,
}

// Reduce a FROM value to a canonical URL + optional ref. The canonical
// harness reference is github.com/<owner>/<repo>[:tag|@ref]; we expand the
// short form to its full https URL and peel off the tag/ref. Anything that
// does not match the GitHub short form is treated as a literal identifier
// (the builder looks it up under the runtime directory).
fn parse_from_value(value: &str) -> NormalizedFrom {
    if is_github_short(value) {
        let (url, ref_) = split_github_ref(value);
        NormalizedFrom { url, ref_ }
    } else {
        NormalizedFrom {
            url: value.to_owned(),
            ref_: None,
        }
    }
}

fn classify_path(_line: usize, display: &str, path: PathBuf) -> Result<ParsedSource, ImageError> {
    let lower = display.to_ascii_lowercase();
    let looks_like_archive = lower.ends_with(".tar")
        || lower.ends_with(".tar.gz")
        || lower.ends_with(".tgz")
        || lower.ends_with(".tar.xz")
        || lower.ends_with(".txz");
    if looks_like_archive {
        return Ok(ParsedSource::Tarball {
            url: path.to_string_lossy().into_owned(),
            local: true,
        });
    }
    Ok(ParsedSource::LocalDir { path })
}

fn is_github_short(token: &str) -> bool {
    // We accept `host/owner/repo` with exactly three slash-separated parts
    // where the host is one of the well-known git hosts. A leading `@` is
    // not allowed (reserved for scoped package names). The third segment
    // may carry a `:tag` or `@ref` suffix; we only check that the bare
    // repository name is non-empty.
    let parts: Vec<&str> = token.split('/').collect();
    if parts.len() != 3 || parts[0].is_empty() {
        return false;
    }
    if parts[0].starts_with('@') {
        return false;
    }
    let host = parts[0];
    let owner = parts[1];
    let repo_segment = parts[2];
    if owner.is_empty() {
        return false;
    }
    let repo_name = repo_segment
        .split([':', '@'])
        .next()
        .unwrap_or("");
    if repo_name.is_empty() {
        return false;
    }
    matches!(host, "github.com" | "gitlab.com" | "bitbucket.org")
}

fn split_github_ref(token: &str) -> (String, Option<String>) {
    // Accept `host/owner/repo`, `host/owner/repo:tag`, `host/owner/repo@ref`.
    // If both `@` and `:` appear, the last separator wins.
    let last_at = token.rfind('@');
    let last_colon = token.rfind(':');
    let (cut_position, ref_kind) = match (last_at, last_colon) {
        (Some(at), Some(colon)) if at > colon => (at, '@'),
        (Some(_), Some(colon)) => (colon, ':'),
        (Some(at), None) => (at, '@'),
        (None, Some(colon)) => (colon, ':'),
        (None, None) => return (format!("https://{token}"), None),
    };
    let (head, tail) = token.split_at(cut_position);
    let ref_ = tail.trim_start_matches(ref_kind).to_string();
    if ref_.is_empty() {
        return (format!("https://{head}"), None);
    }
    (format!("https://{head}"), Some(ref_))
}

fn split_at_ref(token: &str) -> (&str, Option<String>) {
    if let Some(at) = token.rfind('@') {
        // The caller has already peeled off any leading `@scope/`, so the
        // `@` we find here is unambiguously a version separator.
        let (name, version) = token.split_at(at);
        let version = version.trim_start_matches('@').to_string();
        if !version.is_empty() {
            return (name, Some(version));
        }
        return (name, None);
    }
    if let Some(colon) = token.rfind(':') {
        let (name, version) = token.split_at(colon);
        let version = version.trim_start_matches(':').to_string();
        if !version.is_empty() && looks_like_version(&version) {
            return (name, Some(version));
        }
    }
    (token, None)
}

/// Split an optional `@scope/` prefix off a bare-name token, returning the
/// rest as a `&str` slice so callers can avoid re-borrowing.
fn split_scope_token(token: &str) -> (Option<String>, &str) {
    if let Some(rest) = token.strip_prefix('@') {
        if let Some(slash) = rest.find('/') {
            let (scope, after) = rest.split_at(slash);
            // Skip the leading `/` from `after`.
            let after = &after[1..];
            return (Some(scope.to_string()), after);
        }
        // `@scope` with no package name; treat as no scope and let the
        // empty-name guard downstream reject it.
        return (None, token);
    }
    (None, token)
}

fn looks_like_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_dir() -> PathBuf {
        PathBuf::from("/tmp")
    }

    #[test]
    fn parse_minimal_script() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.harness_url, "https://github.com/deepseek-ai/deepseek-harness");
        assert_eq!(script.harness_ref.as_deref(), Some("latest"));
        assert_eq!(script.base_template, None);
        assert_eq!(script.profile, "web");
        assert_eq!(script.name, "image");
        assert_eq!(script.version, "latest");
        assert!(script.ops.is_empty());
    }

    #[test]
    fn parse_from_local_template_name() {
        let script = parse_script(
            "FROM web-base\nPROFILE web\nADD plugin cordis-plugin-foo\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.base_template.as_deref(), Some("web-base"));
        assert_eq!(script.harness_url, "");
        assert_eq!(script.harness_ref, None);
        assert_eq!(script.ops.len(), 1);
        // Scoped names are never mistaken for template references.
        let script = parse_script(
            "FROM @scope/base\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.base_template.as_deref(), Some("@scope/base"));
    }

    #[test]
    fn parse_from_missing_is_an_error() {
        assert!(parse_script("PROFILE web\n", &base_dir()).is_err());
    }

    #[test]
    fn parse_full_script() {
        let script = parse_script(
            "# a comment\n\
             FROM github.com/deepseek-ai/deepseek-harness:latest\n\
             PROFILE web\n\
             NAME team-stack\n\
             VERSION 2.1.0\n\
             LABEL maintainer=alice@example.com\n\
             ADD plugin github.com/team/cordis-plugin-foo:1.2.3\n\
             ADD plugin https://intranet/foo.tgz\n\
             ADD plugin ./plugins/secret\n\
             ADD plugin cordis-plugin-bar@1.0.0\n\
             ADD skill team-conventions\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.name, "team-stack");
        assert_eq!(script.version, "2.1.0");
        assert_eq!(
            script.labels.get("maintainer"),
            Some(&"alice@example.com".to_string())
        );
        assert_eq!(script.ops.len(), 5);
        match &script.ops[0].op_for_test() {
            ParsedSource::Github { url, ref_ } => {
                assert_eq!(url, "https://github.com/team/cordis-plugin-foo");
                assert_eq!(ref_.as_deref(), Some("1.2.3"));
            }
            _ => panic!("expected github source"),
        }
        match &script.ops[1].op_for_test() {
            ParsedSource::Tarball { url, local } => {
                assert_eq!(url, "https://intranet/foo.tgz");
                assert!(!local);
            }
            _ => panic!("expected tarball source"),
        }
        match &script.ops[2].op_for_test() {
            ParsedSource::LocalDir { path } => {
                assert!(path.ends_with("plugins/secret"));
            }
            _ => panic!("expected local dir source"),
        }
        match &script.ops[3].op_for_test() {
            ParsedSource::BareName { name, scope, version } => {
                assert_eq!(name, "cordis-plugin-bar");
                assert!(scope.is_none());
                assert_eq!(version.as_deref(), Some("1.0.0"));
            }
            _ => panic!("expected bare name source"),
        }
        let ImageOp::Add { kind, .. } = &script.ops[4];
        assert!(matches!(kind, AddKind::Skill));
    }

    #[test]
    fn from_github_short_form_keeps_latest_tag() {
        // `:latest` is preserved in the parsed ref so templates list and
        // version picker can show the source; the clone path interprets it
        // as the repository HEAD separately.
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.harness_url, "https://github.com/deepseek-ai/deepseek-harness");
        assert_eq!(script.harness_ref.as_deref(), Some("latest"));
    }

    #[test]
    fn from_github_short_form_with_at_ref_extracts_ref() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness@v0.1.0
PROFILE web
",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.harness_ref.as_deref(), Some("v0.1.0"));
    }

    #[test]
    fn from_literal_value_names_a_local_template() {
        // A non-GitHub FROM value is a local template name (e.g. the base
        // template for a version); the builder resolves it under
        // `<root>/templates/`.
        let script = parse_script(
            "FROM v0.2.0
PROFILE web
",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.base_template.as_deref(), Some("v0.2.0"));
        assert_eq!(script.harness_url, "");
        assert!(script.harness_ref.is_none());
    }

    #[test]
    fn harness_digest_is_stable() {
        let a = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        let b = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE headless\n",
            &base_dir(),
        )
        .unwrap();
        // Same FROM value yields the same digest regardless of the rest of
        // the script (the digest is purely a function of the URL + ref).
        assert_eq!(a.harness_digest(), b.harness_digest());
    }

    #[test]
    fn harness_digest_differs_on_ref() {
        let a = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        let b = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:v0.1.0\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        assert_ne!(a.harness_digest(), b.harness_digest());
    }

    #[test]
    fn missing_from_is_reported() {
        let error = parse_script("PROFILE web\n", &base_dir()).unwrap_err();
        match error {
            ImageError::MissingDirective { name, .. } => assert_eq!(name, "FROM"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn github_short_form_with_at_ref() {
        let source = parse_source_token(1, "github.com/team/repo@v1.0.0", &base_dir()).unwrap();
        match source {
            ParsedSource::Github { url, ref_ } => {
                assert_eq!(url, "https://github.com/team/repo");
                assert_eq!(ref_.as_deref(), Some("v1.0.0"));
            }
            _ => panic!("expected github source"),
        }
    }

    #[test]
    fn relative_path_becomes_local_dir() {
        let source = parse_source_token(2, "./plugins/secret", &PathBuf::from("/work")).unwrap();
        match source {
            ParsedSource::LocalDir { path } => {
                assert!(path.ends_with("plugins/secret"));
            }
            _ => panic!("expected local dir"),
        }
    }

    #[test]
    fn bare_name_with_scope_and_version() {
        let source = parse_source_token(
            3,
            "@scope/cordis-plugin-bar:1.2.3",
            &base_dir(),
        )
        .unwrap();
        match source {
            ParsedSource::BareName { name, scope, version } => {
                assert_eq!(scope.as_deref(), Some("scope"));
                assert_eq!(name, "cordis-plugin-bar");
                assert_eq!(version.as_deref(), Some("1.2.3"));
            }
            _ => panic!("expected bare name"),
        }
    }

    #[test]
    fn add_data_kind_is_accepted() {
        // `ADD data` marks a non-code payload: the parser must accept it and
        // carry the kind through to the manifest as `resourceType: data`.
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\nADD data thing\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.ops.len(), 1);
        let ImageOp::Add { kind, source, .. } = &script.ops[0];
        assert!(matches!(kind, AddKind::Data));
        match source {
            ParsedSource::BareName { name, .. } => assert_eq!(name, "thing"),
            _ => panic!("expected bare name source"),
        }
    }

    #[test]
    fn cp_alias_is_accepted_and_defaults_to_plugin() {
        // `CP` is the legacy alias for `ADD plugin`. Old scripts with a bare
        // source token continue to parse and get the Plugin kind.
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\nCP cordis-plugin-foo\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.ops.len(), 1);
        let ImageOp::Add { kind, source, .. } = &script.ops[0];
        assert!(matches!(kind, AddKind::Plugin));
        match source {
            ParsedSource::BareName { name, .. } => assert_eq!(name, "cordis-plugin-foo"),
            _ => panic!("expected bare name source"),
        }
    }

    // Test-only accessor for the variant's source. Avoids a `pub` field on
    // the production enum (we keep its variants private to the parser).
    impl ImageOp {
        pub(crate) fn op_for_test(&self) -> &ParsedSource {
            match self {
                ImageOp::Add { source, .. } => source,
            }
        }
    }
}
