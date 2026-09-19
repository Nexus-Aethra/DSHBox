//! Cordis service-dependency graph for a DSH template or container.
//!
//! DSH is built on cordis, where "everything is a plugin" and plugins find each
//! other through *services* rather than through imports: a plugin declares the
//! services it waits for (`inject`) and the services it registers (`ctx`/`serve`),
//! and cordis simply leaves a plugin pending until its dependencies exist. That
//! makes the interesting relation in DSH a bipartite graph
//!
//! ```text
//! plugin ──requires──▶ service ◀──provides── plugin
//! ```
//!
//! from which a directed plugin-to-plugin edge is derived. This crate extracts
//! that graph from a template's or container's on-disk tree and reports the three
//! failures cordis itself only shows as a hang: a required service nobody
//! provides, a provider that is installed but not activated, and a dependency
//! cycle.
//!
//! Deliberately out of scope: npm-level package dependencies. The user-facing
//! graph is plugin-to-plugin, so a plugin's library dependencies are not nodes
//! and the profile lockfile is not consulted.
//!
//! The scanner is text-based, because the workspace has no TypeScript parser. The
//! forms it recognises were derived from a real harness checkout; see
//! [`extract`] for the rule set and the false positives it avoids.

pub mod extract;
pub mod graph;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use extract::Scan;
use graph::Discovered;

/// Largest source file read while scanning. Real plugin sources are a few
/// kilobytes; a bundled file above this is minified output and contributes
/// nothing a literal scan can trust.
const MAX_SOURCE_BYTES: u64 = 512 * 1024;

/// Most source files read per package, so one pathological package cannot make
/// the read path unbounded.
const MAX_SOURCE_FILES: usize = 400;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GraphSource {
    Template,
    Container,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GraphPlugin {
    pub name: String,
    pub version: Option<String>,
    /// Whether the plugin is part of the profile's activation closure. Only an
    /// activated plugin is actually loaded at runtime, which is why a service
    /// provided solely from outside this set can never be satisfied.
    pub activated: bool,
    /// Display path the declarations were read from.
    pub source: String,
    pub provides: Vec<String>,
    pub requires: Vec<String>,
}

/// One plugin's relation to one service.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEdge {
    pub plugin: String,
    pub service: String,
}

/// A service name that more than one loaded plugin registers. cordis resolves a
/// service name to a single provider, so the extra registrations are a wiring
/// conflict — and the traceable cause of both the fan-out a reader sees in the
/// diagram and most of the cycles reported for it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceConflict {
    pub service: String,
    /// The activated providers of `service`, in name order.
    pub providers: Vec<String>,
}

/// A derived plugin-to-plugin edge: `from` requires `service`, which `to`
/// provides.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginLink {
    pub from: String,
    pub to: String,
    pub service: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginGraph {
    pub source: GraphSource,
    /// Template name or container id.
    pub source_id: String,
    pub profile: String,
    pub plugins: Vec<GraphPlugin>,
    /// Every service named anywhere in the graph, required or provided.
    pub services: Vec<String>,
    pub requires: Vec<ServiceEdge>,
    pub provides: Vec<ServiceEdge>,
    /// Derived plugin-to-plugin edges: `from` requires `service`, which `to`
    /// provides. Derived from the declarations alone, so a provider outside the
    /// activation closure still appears here; that pairing is what
    /// [`PluginGraph::inactive_providers`] exists to call out.
    pub links: Vec<PluginLink>,
    /// Dependency order — every plugin in the graph, activated or not, with
    /// dependencies before dependents. This is the order the declarations imply,
    /// not a replacement for `dsh.profile.bundles`, which is the list actually
    /// loaded. Members of a cycle have no order among themselves and are emitted
    /// as one block at the position their dependencies put them; `cycles` names
    /// them, and they are the entries cordis leaves pending.
    pub order: Vec<String>,
    /// Groups of plugins that depend on each other. cordis leaves these
    /// silently pending; here they are named.
    pub cycles: Vec<Vec<String>>,
    /// Required services no discovered plugin provides.
    pub missing: Vec<ServiceEdge>,
    /// Services required by an activated plugin whose only providers are outside
    /// the activation closure — installed but never loaded.
    pub inactive_providers: Vec<ServiceEdge>,
    /// Services registered by more than one activated plugin. Reported because a
    /// reader seeing a cycle or a hairball cannot tell from the drawing alone
    /// that the same service name has several owners.
    pub conflicts: Vec<ServiceConflict>,
    pub diagnostics: Vec<String>,
    pub scanned_at: u64,
}

/// Filesystem roots to scan. Every field is optional so a partially materialised
/// template or a container built from a legacy base still yields whatever is
/// available instead of failing outright.
#[derive(Clone, Debug, Default)]
pub struct ScanRoots {
    /// DSH harness source tree, whose `packages/` subtree holds the core plugins.
    pub harness: Option<PathBuf>,
    /// `<root>/profile/profiles/<profile>`: the profile manifest and, in a
    /// container, the installed third-party plugins.
    pub profile: Option<PathBuf>,
    /// `<runtime>/repository`: imported third-party plugin sources.
    pub repository: Option<PathBuf>,
}

/// Read the plugin graph for one template or container.
pub fn build_graph(
    source: GraphSource,
    source_id: &str,
    profile: &str,
    roots: &ScanRoots,
    scanned_at: u64,
) -> PluginGraph {
    let mut diagnostics: Vec<String> = Vec::new();
    let mut candidates: Vec<Candidate> = Vec::new();

    if let Some(harness) = &roots.harness {
        let packages = harness.join("packages");
        if packages.is_dir() {
            collect_packages(&packages, 3, &mut candidates);
        } else {
            diagnostics.push(format!("harness has no packages directory: {}", packages.display()));
        }
        // DSH keeps its cordis framework plugins in `vendor/`, one level deep —
        // loader, group, include, timer, hmr (all `@deepseek-ai/cordis-plugin-*`).
        // They provide services the rest of the graph requires, so skipping this
        // root reported the loader service as missing for eight plugins while the
        // package providing it sat on disk.
        let vendor = harness.join("vendor");
        if vendor.is_dir() {
            collect_packages(&vendor, 2, &mut candidates);
        }
    }
    if let Some(profile_dir) = &roots.profile {
        let modules = profile_dir.join("node_modules");
        if modules.is_dir() {
            collect_modules(&modules, &mut candidates);
        }
    }
    if let Some(repository) = &roots.repository {
        let plugins = repository.join("plugins");
        if plugins.is_dir() {
            collect_repository(&plugins, &mut candidates);
        }
    }

    // One package can be reached through more than one root (a repository import
    // is also copied into the profile); keep the first sighting so a plugin is a
    // single node.
    let mut unique: BTreeMap<String, Candidate> = BTreeMap::new();
    for candidate in candidates {
        unique.entry(candidate.name.clone()).or_insert(candidate);
    }

    // `None` when the profile tree is absent, e.g. a prepared Harness base: the
    // graph then reports activation as unknown rather than as empty.
    let activated = activation(roots.profile.as_deref(), &unique, &mut diagnostics);

    let mut discovered: Vec<Discovered> = Vec::new();
    for candidate in unique.into_values() {
        let files = source_files(&candidate.directory, &mut diagnostics);
        let mut scan = Scan::default();
        for file in files {
            match fs::read_to_string(&file) {
                Ok(text) => {
                    let next = extract::scan_source(&text);
                    // A declaration site that could not be resolved is only
                    // actionable with its file, and a package holds many, so the
                    // note is qualified here rather than in the scanner, which
                    // has no idea which file it was handed.
                    let label = file.strip_prefix(&candidate.directory).unwrap_or(&file);
                    for unresolved in &next.unresolved {
                        diagnostics.push(format!(
                            "{}: {}: {unresolved}",
                            candidate.name,
                            display_path(label)
                        ));
                    }
                    scan_merge(&mut scan, next);
                }
                Err(error) => {
                    diagnostics.push(format!("cannot read {}: {error}", display_path(&file)))
                }
            }
        }
        let has_declarations = !scan.provides.is_empty()
            || !scan.requires.is_empty()
            || !scan.unresolved.is_empty();
        // Keep packages that declare something, plus whatever the profile
        // activates: a bundle named in the boxfile is worth showing even when it
        // only aggregates other plugins.
        if has_declarations
            || activated
                .as_ref()
                .is_some_and(|names| names.contains(&candidate.name))
        {
            discovered.push(Discovered {
                name: candidate.name.clone(),
                version: candidate.version.clone(),
                source: candidate.display,
                scan,
            });
        }
    }
    discovered.sort_by(|left, right| left.name.cmp(&right.name));

    graph::assemble(
        source,
        source_id.to_owned(),
        profile.to_owned(),
        activated,
        discovered,
        diagnostics,
        scanned_at,
    )
}

fn scan_merge(total: &mut Scan, next: Scan) {
    total.requires.extend(next.requires);
    total.provides.extend(next.provides);
    total.catalogue.extend(next.catalogue);
    total.unresolved.extend(next.unresolved);
}

/// One plugin package found on disk, before its sources are read.
#[derive(Clone, Debug)]
struct Candidate {
    name: String,
    version: Option<String>,
    directory: PathBuf,
    display: String,
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Read `name` and `version` from a `package.json`. Returns `None` for a
/// directory that is not a package.
fn read_manifest(directory: &Path) -> Option<(String, Option<String>)> {
    let text = fs::read_to_string(directory.join("package.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = value.get("name")?.as_str()?.to_owned();
    let version = value
        .get("version")
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    Some((name, version))
}

/// Package directory names that never hold plugin sources worth scanning.
fn is_skipped_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules" | "dist" | "lib" | "build" | "coverage" | ".git" | ".cache"
    ) || name == "tests"
        || name == "test"
        || name == "__tests__"
}

/// Collect core plugin packages under `<harness>/packages`, which groups them one
/// or two directories deep (`packages/<group>/<package>`).
fn collect_packages(directory: &Path, depth: usize, candidates: &mut Vec<Candidate>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_skipped_dir(&name) || name.starts_with('.') {
            continue;
        }
        if let Some((package, version)) = read_manifest(&path) {
            candidates.push(Candidate {
                display: short_display(&path),
                name: package,
                version,
                directory: path,
            });
            continue;
        }
        if depth > 1 {
            collect_packages(&path, depth - 1, candidates);
        }
    }
}

/// Collect installed plugins from a profile's `node_modules`, one and two levels
/// deep so `@scope/name` is reachable.
fn collect_modules(modules: &Path, candidates: &mut Vec<Candidate>) {
    let Ok(entries) = fs::read_dir(modules) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if name.starts_with('@') {
            if let Ok(scoped) = fs::read_dir(&path) {
                for inner in scoped.flatten() {
                    let inner_path = inner.path();
                    if inner_path.is_dir() {
                        if let Some((package, version)) = read_manifest(&inner_path) {
                            candidates.push(Candidate {
                                display: short_display(&inner_path),
                                name: package,
                                version,
                                directory: inner_path,
                            });
                        }
                    }
                }
            }
            continue;
        }
        if let Some((package, version)) = read_manifest(&path) {
            candidates.push(Candidate {
                display: short_display(&path),
                name: package,
                version,
                directory: path,
            });
        }
    }
}

/// Collect imported plugin sources from `<runtime>/repository/plugins/<id>/source`.
fn collect_repository(plugins: &Path, candidates: &mut Vec<Candidate>) {
    let Ok(entries) = fs::read_dir(plugins) else {
        return;
    };
    for entry in entries.flatten() {
        let source = entry.path().join("source");
        if !source.is_dir() {
            continue;
        }
        if let Some((package, version)) = read_manifest(&source) {
            candidates.push(Candidate {
                display: short_display(&source),
                name: package,
                version,
                directory: source,
            });
        }
    }
}

/// Last two path components, which is enough to identify a package in the UI
/// without leaking an absolute machine path into the graph.
fn short_display(path: &Path) -> String {
    let mut parts: Vec<String> = path
        .components()
        .rev()
        .take(2)
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.reverse();
    parts.join("/")
}

/// Source files to scan inside one package, preferring `src/` when it exists.
fn source_files(package: &Path, diagnostics: &mut Vec<String>) -> Vec<PathBuf> {
    let start = if package.join("src").is_dir() {
        package.join("src")
    } else {
        package.to_path_buf()
    };
    let mut files = Vec::new();
    let mut discovered = 0usize;
    collect_sources(&start, &mut files, &mut discovered);
    if discovered > files.len() {
        diagnostics.push(format!(
            "{} has {discovered} source files; scanning the first {}",
            short_display(package),
            files.len()
        ));
    }
    files.sort();
    files
}

/// Walk for source files, counting everything found but only keeping the first
/// [`MAX_SOURCE_FILES`] so one pathological package cannot make the read path
/// unbounded. The count still reflects the whole tree, so the diagnostic that
/// reports truncation does not claim to have seen exactly the cap.
fn collect_sources(directory: &Path, files: &mut Vec<PathBuf>, discovered: &mut usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if is_skipped_dir(&name) || name.starts_with('.') {
                continue;
            }
            collect_sources(&path, files, discovered);
            continue;
        }
        if name.ends_with(".d.ts") || !is_source_file(&name) {
            continue;
        }
        match entry.metadata() {
            Ok(metadata) if metadata.len() > MAX_SOURCE_BYTES => continue,
            Ok(_) => {
                *discovered += 1;
                if files.len() < MAX_SOURCE_FILES {
                    files.push(path);
                }
            }
            Err(_) => continue,
        }
    }
}

fn is_source_file(name: &str) -> bool {
    [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
        .iter()
        .any(|extension| name.ends_with(extension))
}

/// One row of a cordis patch list: what it inserts, and whether it is switched off.
#[derive(Default)]
struct PatchRow {
    name: Option<String>,
    disabled: bool,
}

/// The plugins a profile actually loads.
///
/// No single file says this, so two sources are combined:
///
/// * the **patch layers** — every `dsh.profile.bundles` entry contributes a
///   `cordis.patch.yml`, the profile has one of its own, and later layers win per
///   row id, which is what makes `disabled: true` authoritative (the web bundle
///   switches off 24 rows, `tool-bash` and `tool-fs` among them);
/// * the **dependency closure** of those bundles, because DSH also mounts entries
///   no patch file mentions — app-boot mounts the loader, the include and group
///   builtins, and the daemon adds Box's own context plugin as a launcher overlay.
///
/// Using the closure alone counted packages that are only dependencies and never
/// applied `disabled`. Using the patch files alone missed every builtin: it called
/// 142 real providers unloaded. The union over-counts slightly and agrees with the
/// one source that describes the live tree — DSH's own startup audit, which
/// reports every failed, missing or pending entry and reports nothing at all for
/// this container.
///
/// Launcher `--patch` overlays are not read; Box's overlay only rewrites a row's
/// config, so it changes no plugin's activation.
///
/// `None` means activation cannot be known from what is on disk — a prepared
/// Harness base carries no profile tree until something is built from it. Callers
/// must not read that as "nothing is activated": doing so reported every provider
/// in the graph as an inactive provider.
fn activation(
    profile: Option<&Path>,
    candidates: &BTreeMap<String, Candidate>,
    diagnostics: &mut Vec<String>,
) -> Option<BTreeSet<String>> {
    let profile = profile?;
    let manifest = profile.join("package.json");
    let Ok(text) = fs::read_to_string(&manifest) else {
        diagnostics.push(format!("no profile manifest at {}", display_path(&manifest)));
        return None;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        diagnostics.push(format!(
            "profile manifest is not valid JSON: {}",
            display_path(&manifest)
        ));
        return None;
    };
    // No bundle list is not an empty bundle list: it means this is not a profile
    // manifest this scanner understands.
    let bundles = value
        .pointer("/dsh/profile/bundles")
        .and_then(|value| value.as_array())?;

    let mut rows: BTreeMap<String, PatchRow> = BTreeMap::new();
    for bundle in bundles.iter().filter_map(|value| value.as_str()) {
        match candidates.get(bundle) {
            Some(candidate) => {
                let patch = candidate.directory.join("cordis.patch.yml");
                match fs::read_to_string(&patch) {
                    Ok(text) => apply_patch_layer(&text, &mut rows, diagnostics, &patch),
                    Err(error) => diagnostics.push(format!(
                        "cannot read {}: {error}",
                        display_path(&patch)
                    )),
                }
            }
            None => diagnostics.push(format!(
                "profile bundle {bundle} is not installed, so the plugins it inserts are unknown"
            )),
        }
    }
    // The profile's own layer is applied last and may insert as well as patch.
    let own = profile.join("cordis.patch.yml");
    if own.is_file() {
        match fs::read_to_string(&own) {
            Ok(text) => apply_patch_layer(&text, &mut rows, diagnostics, &own),
            Err(error) => diagnostics.push(format!("cannot read {}: {error}", display_path(&own))),
        }
    }
    // Dependency closure of the bundles: entries DSH mounts without a patch row
    // (the loader, the include and group builtins, Box's context plugin) are only
    // reachable this way.
    let dependencies: BTreeMap<String, Vec<String>> = candidates
        .iter()
        .map(|(name, candidate)| (name.clone(), read_dependencies(&candidate.directory)))
        .collect();
    let mut closure: BTreeSet<String> = BTreeSet::new();
    let mut pending: Vec<String> = bundles
        .iter()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect();
    while let Some(name) = pending.pop() {
        if !closure.insert(name.clone()) {
            continue;
        }
        if let Some(next) = dependencies.get(&name) {
            pending.extend(next.iter().cloned());
        }
    }

    // Rows carry the only authoritative exclusion: a package switched off by a
    // later layer must not be reported as loaded even when the closure reaches it.
    let mut disabled: BTreeSet<String> = BTreeSet::new();
    let mut inserted: BTreeSet<String> = BTreeSet::new();
    for row in rows.into_values() {
        let Some(name) = row.name else { continue };
        if row.disabled {
            disabled.insert(name);
        } else {
            inserted.insert(name);
        }
    }
    closure.extend(inserted);
    Some(closure.difference(&disabled).cloned().collect())
}

fn read_dependencies(package: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(package.join("package.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for key in ["dependencies", "peerDependencies"] {
        if let Some(map) = value.get(key).and_then(|value| value.as_object()) {
            names.extend(map.keys().cloned());
        }
    }
    names
}

/// Apply one patch list. A row either carries an `insert` list of new rows, or is
/// itself a patch addressed at an existing row by `id` — disabling it, or
/// replacing its config.
fn apply_patch_layer(
    text: &str,
    rows: &mut BTreeMap<String, PatchRow>,
    diagnostics: &mut Vec<String>,
    file: &Path,
) {
    let Ok(document) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        diagnostics.push(format!("cannot parse {} as YAML", display_path(file)));
        return;
    };
    let Some(list) = document.as_sequence() else {
        diagnostics.push(format!("{} is not a patch list", display_path(file)));
        return;
    };
    for row in list {
        match row.get("insert").and_then(|value| value.as_sequence()) {
            Some(inserted) => {
                for entry in inserted {
                    merge_patch_row(entry, rows);
                }
            }
            // A patch row without an id cannot be addressed at anything.
            None => merge_patch_row(row, rows),
        }
    }
}

fn merge_patch_row(entry: &serde_yaml::Value, rows: &mut BTreeMap<String, PatchRow>) {
    let Some(id) = entry.get("id").and_then(|value| value.as_str()) else {
        return;
    };
    let row = rows.entry(id.to_owned()).or_default();
    if let Some(name) = entry.get("name").and_then(|value| value.as_str()) {
        row.name = Some(name.to_owned());
    }
    // `disabled` may also be an expression DSH evaluates; an unreadable one is not
    // a reason to claim the plugin is off.
    if let Some(disabled) = entry.get("disabled").and_then(|value| value.as_bool()) {
        row.disabled = disabled;
    }
}

#[cfg(test)]
mod tests;
