//! Finding a container's resources: the built-in kinds, what a plugin declares,
//! and — when it declares nothing — the paths its shipped code composes.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::kinds::{builtin, builtins, ResolvedKind, Shape};
use crate::transfer::tree_stats;

/// One entry of a plugin's `dshbox.resources` declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredResource {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    /// Container-relative path the plugin persists to.
    pub path: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub shape: Option<String>,
    /// How deep the independent entries sit under `path` (default 1).
    #[serde(default)]
    pub depth: Option<u8>,
    #[serde(default)]
    pub description: Option<String>,
}


/// A plugin thinks in DSH-home paths (`dshell/ssh`); the container keeps that
/// home at `profile/`, so declarations are stored container-relative.
fn container_path(path: &str) -> String {
    if path.starts_with("profile/") || path.starts_with("./profile/") {
        path.to_owned()
    } else {
        format!("profile/{path}")
    }
}

/// Where a discovered kind came from, which is also how much it can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Box ships this kind.
    Builtin,
    /// The plugin package declares it.
    Declared,
    /// Box read it out of the plugin's code; the user confirms before use.
    Inferred,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovered {
    pub kind: ResolvedKind,
    pub scope: Scope,
    /// The package the resource belongs to, when it was found through one.
    pub plugin: Option<String>,
    pub description: Option<String>,
    /// What is on disk right now.
    pub exists: bool,
    pub bytes: u64,
    pub files: u64,
}

/// Read `dshbox.resources` out of a parsed `package.json`.
pub fn parse_declarations(package_json: &str) -> Result<Vec<DeclaredResource>, String> {
    let value: serde_json::Value =
        serde_json::from_str(package_json).map_err(|error| format!("invalid package.json: {error}"))?;
    let Some(resources) = value.get("dshbox").and_then(|dshbox| dshbox.get("resources")) else {
        return Ok(Vec::new());
    };
    let list = if resources.is_array() {
        resources.clone()
    } else {
        // `"resources": { "sessions": { ... } }` — the map form keys by id.
        let Some(map) = resources.as_object() else {
            return Err("dshbox.resources must be an array or an object".to_owned());
        };
        let mut list = Vec::new();
        for (id, entry) in map {
            let mut entry = entry.clone();
            if let Some(object) = entry.as_object_mut() {
                object.entry("id").or_insert_with(|| serde_json::Value::String(id.clone()));
            }
            list.push(entry);
        }
        serde_json::Value::Array(list)
    };
    let declared: Vec<DeclaredResource> = serde_json::from_value(list)
        .map_err(|error| format!("invalid dshbox.resources entry: {error}"))?;
    for resource in &declared {
        if resource.path.trim().is_empty() {
            return Err(format!("resource `{}` declares an empty path", resource.id));
        }
    }
    Ok(declared)
}

/// Declarations of one installed package: `dshbox.resources` in its
/// `package.json`, or a `dshbox.resources.json` beside it.
pub fn declared_in_package(package_dir: &Path) -> Result<Vec<DeclaredResource>, String> {
    let manifest = package_dir.join("package.json");
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        let declared = parse_declarations(&text)?;
        if !declared.is_empty() {
            return Ok(declared);
        }
    }
    let sibling = package_dir.join("dshbox.resources.json");
    if let Ok(text) = std::fs::read_to_string(&sibling) {
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("invalid {}: {error}", sibling.display()))?;
        let list = value.get("resources").cloned().unwrap_or(value);
        return serde_json::from_value(list)
            .map_err(|error| format!("invalid {}: {error}", sibling.display()));
    }
    Ok(Vec::new())
}

/// Paths a plugin's code composes under its home/profile, as candidates.
///
/// This is a text scan, not an evaluation: it reads `join(root, 'a', 'b')`
/// chains whose first argument is an identifier (so the path is relative to
/// something) and keeps the literal tail. Anything else — `${…}` templates,
/// computed arguments — ends the chain, because the scan must never invent a
/// path component it did not literally read.
pub fn scanned_candidates(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut candidates = Vec::new();
    let mut index = 0usize;
    while let Some(offset) = source[index..].find("join(") {
        let start = index + offset + "join(".len();
        let (parts, next) = read_join_arguments(source, bytes, start);
        index = next;
        if parts.len() < 2 {
            continue;
        }
        // The first argument has to be a variable: `join` on a literal would
        // mean the path is absolute already, which is not a container resource.
        if !parts[0].is_identifier {
            continue;
        }
        let literals: Vec<String> = parts
            .iter()
            .skip(1)
            .take_while(|part| part.literal.is_some())
            .filter_map(|part| part.literal.clone())
            .collect();
        if !literals.is_empty() {
            candidates.push(literals.join("/"));
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

struct Arg {
    literal: Option<String>,
    is_identifier: bool,
}

fn read_join_arguments(source: &str, bytes: &[u8], mut at: usize) -> (Vec<Arg>, usize) {
    let mut args = Vec::new();
    let mut depth = 0usize;
    let start = at;
    while at < bytes.len() {
        match bytes[at] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    args.push(classify(&source[start..at]));
                    return (args, at + 1);
                }
                depth -= 1;
            }
            b',' if depth == 0 => {
                args.push(classify(&source[start..at]));
                at += 1;
                return finish(source, bytes, at, args);
            }
            _ => {}
        }
        at += 1;
    }
    (args, at)
}

fn finish(source: &str, bytes: &[u8], mut at: usize, mut args: Vec<Arg>) -> (Vec<Arg>, usize) {
    let start = at;
    let mut depth = 0usize;
    while at < bytes.len() {
        match bytes[at] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                if depth == 0 {
                    args.push(classify(&source[start..at]));
                    return (args, at + 1);
                }
                depth -= 1;
            }
            b',' if depth == 0 => {
                args.push(classify(&source[start..at]));
                at += 1;
                return finish(source, bytes, at, args);
            }
            _ => {}
        }
        at += 1;
    }
    (args, at)
}

fn classify(raw: &str) -> Arg {
    let trimmed = raw.trim();
    let literal = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .or_else(|| trimmed.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')))
        .or_else(|| trimmed.strip_prefix('`').and_then(|rest| rest.strip_suffix('`')))
        .filter(|inner| !inner.contains('$'))
        .map(str::to_owned);
    Arg {
        is_identifier: !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '.'),
        literal,
    }
}

/// Collect the resources of one container. Without a plugin this is the two
/// built-in kinds plus every installed package that declares one; with a plugin
/// it is that package's declarations, or its scanned candidates when it has
/// none.
pub fn discover(container_root: &Path, profile_name: &str, plugin: Option<&str>) -> Vec<Discovered> {
    let profile = container_root.join("profile");
    let mut found: Vec<Discovered> = builtins()
        .iter()
        .map(|kind| {
            let resolved = ResolvedKind::from_builtin(kind);
            measured(container_root, resolved, Scope::Builtin, None, None)
        })
        .collect();

    let modules = profile.join("profiles").join(profile_name).join("node_modules");

    match plugin {
        Some(name) => {
            if let Some(dir) = package_dir(&modules, name) {
                match declared_in_package(&dir) {
                    Ok(declared) if !declared.is_empty() => {
                        for entry in declared {
                            let resolved = ResolvedKind {
                                id: entry.id.clone(),
                                label: entry.label.clone().unwrap_or_else(|| entry.id.clone()),
                                path: container_path(&entry.path),
                                secret: entry.secret,
                                shape: entry
                                    .shape
                                    .as_deref()
                                    .and_then(Shape::parse)
                                    .unwrap_or(Shape::Opaque),
                                entry_depth: entry.depth.unwrap_or(1).max(1),
                                inferred: false,
                            };
                            found.push(measured(
                                container_root,
                                resolved,
                                Scope::Declared,
                                Some(name.to_owned()),
                                entry.description.clone(),
                            ));
                        }
                    }
                    _ => {
                        for path in scan_package(&dir) {
                            let resolved = ResolvedKind {
                                id: path.replace('/', "-"),
                                label: path.clone(),
                                path: container_path(&path),
                                // A key file the code mentions is a secret until
                                // the user says otherwise.
                                secret: path.contains("credential") || path.contains("key"),
                                shape: Shape::Entries,
                                entry_depth: 2,
                                inferred: true,
                            };
                            found.push(measured(
                                container_root,
                                resolved,
                                Scope::Inferred,
                                Some(name.to_owned()),
                                Some(format!("read from {name}'s code")),
                            ));
                        }
                    }
                }
            }
        }
        None => {
            for dir in installed_packages(&modules) {
                let Ok(declared) = declared_in_package(&dir) else {
                    continue;
                };
                let plugin_name = package_name(&modules, &dir);
                for entry in declared {
                    let resolved = ResolvedKind {
                        id: entry.id.clone(),
                        label: entry.label.clone().unwrap_or_else(|| entry.id.clone()),
                        path: container_path(&entry.path),
                        secret: entry.secret,
                        shape: entry
                            .shape
                            .as_deref()
                            .and_then(Shape::parse)
                            .unwrap_or(Shape::Opaque),
                        entry_depth: entry.depth.unwrap_or(1).max(1),
                        inferred: false,
                    };
                    found.push(measured(
                        container_root,
                        resolved,
                        Scope::Declared,
                        plugin_name.clone(),
                        entry.description.clone(),
                    ));
                }
            }
        }
    }

    let mut seen = std::collections::BTreeSet::new();
    found.retain(|entry| seen.insert((entry.kind.id.clone(), entry.kind.path.clone())));
    found
}

/// Scanned candidates that exist on disk: a plugin's code mentioning a path it
/// never wrote is not a resource.
fn scan_package(package_dir: &Path) -> Vec<String> {
    let mut candidates = Vec::new();
    for file in source_files(package_dir) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for candidate in scanned_candidates(&text) {
            candidates.push(candidate);
        }
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name == "node_modules" || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("js" | "mjs" | "cjs")
            ) {
                files.push(path);
            }
        }
    }
    files
}

/// The directory of one installed package, honoring the `@scope/name` form.
fn package_dir(modules: &Path, name: &str) -> Option<PathBuf> {
    let direct = modules.join(name);
    if direct.is_dir() {
        return Some(direct);
    }
    let (scope, bare) = name.split_once('/')?;
    let scoped = modules.join(scope).join(bare);
    scoped.is_dir().then_some(scoped)
}

/// Every package installed in one profile, `@scope/name` form, sorted — what
/// the plugin picker offers. Declared plugins alone would hide the packages a
/// bundle pulled in transitively.
pub fn installed_plugins(container_root: &Path, profile_name: &str) -> Vec<String> {
    let modules = container_root
        .join("profile")
        .join("profiles")
        .join(profile_name)
        .join("node_modules");
    let mut names: Vec<String> = installed_packages(&modules)
        .iter()
        .filter_map(|dir| package_name(&modules, dir))
        .collect();
    names.sort();
    names.dedup();
    names
}

fn installed_packages(modules: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let Ok(entries) = std::fs::read_dir(modules) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('@') {
            if let Ok(scoped) = std::fs::read_dir(&path) {
                for inner in scoped.flatten() {
                    if inner.path().is_dir() {
                        dirs.push(inner.path());
                    }
                }
            }
        } else if name != ".bin" && !name.starts_with('.') {
            dirs.push(path);
        }
    }
    dirs
}

fn package_name(modules: &Path, dir: &Path) -> Option<String> {
    let relative = dir.strip_prefix(modules).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().to_string())
        .collect();
    Some(parts.join("/"))
}

fn measured(
    root: &Path,
    kind: ResolvedKind,
    scope: Scope,
    plugin: Option<String>,
    description: Option<String>,
) -> Discovered {
    let path = crate::transfer::safe_join(root, &kind.path).ok();
    let (bytes, files) = path
        .as_deref()
        .filter(|path| path.exists())
        .and_then(|path| tree_stats(path).ok())
        .unwrap_or((0, 0));
    Discovered {
        exists: path.as_deref().is_some_and(|path| path.exists()),
        kind,
        scope,
        plugin,
        description,
        bytes,
        files,
    }
}

/// Resolve a kind id against the built-ins and one plugin's declarations.
pub fn resolve(kind_id: &str, declared: &[DeclaredResource]) -> Option<ResolvedKind> {
    if let Some(kind) = builtin(kind_id) {
        return Some(ResolvedKind::from_builtin(kind));
    }
    declared
        .iter()
        .find(|entry| entry.id == kind_id)
        .map(|entry| ResolvedKind {
            id: entry.id.clone(),
            label: entry.label.clone().unwrap_or_else(|| entry.id.clone()),
            path: container_path(&entry.path),
            secret: entry.secret,
            shape: entry
                .shape
                .as_deref()
                .and_then(Shape::parse)
                .unwrap_or(Shape::Opaque),
            entry_depth: entry.depth.unwrap_or(1).max(1),
            inferred: false,
        })
}
