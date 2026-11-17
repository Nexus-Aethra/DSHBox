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
//! Plugin-to-plugin is the graph; npm-level packaging is [`lockfile`]. A sealed
//! template keeps no `node_modules`, so the profile's `pnpm-lock.yaml` is the
//! only record of what a container will install — including the plugins a bundle
//! pulled in that no boxfile names — and it is where the resolved versions come
//! from.
//!
//! The scanner is text-based, because the workspace has no TypeScript parser. The
//! forms it recognises were derived from a real harness checkout; see
//! [`extract`] for the rule set and the false positives it avoids.

pub mod extract;
pub mod graph;
pub mod lockfile;

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

/// Which cordis application a plugin is mounted in.
///
/// A package can ship both: `dsh.client` in its manifest declares a browser
/// entry (`exports["./client"]`), and DSH's own docs call the two the package's
/// host half and its client half. They are separate plugins in separate contexts
/// — `sessions` is registered by `dsh-session` in the host app and by
/// `dsh-api-session-controller/src/client/**` in the browser — so a graph node
/// per package would weld two plugins together and draw edges across contexts
/// that no cordis scope has.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Half {
    Host,
    Client,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GraphPlugin {
    /// Node key. Equal to `name` for a package with one half; the client half of
    /// a package that also has a host half is suffixed, because both nodes then
    /// exist and a bare package name would be ambiguous.
    pub id: String,
    /// Package name. Two nodes can share it — see [`Half`].
    pub name: String,
    pub half: Half,
    pub version: Option<String>,
    /// Whether the plugin is part of the profile's activation closure. Only an
    /// activated plugin is actually loaded at runtime, which is why a service
    /// provided solely from outside this set can never be satisfied.
    pub activated: bool,
    /// Display path the declarations were read from.
    pub source: String,
    pub provides: Vec<String>,
    pub requires: Vec<String>,
    /// Plugin names this package's patch file inserts — the contents of an
    /// umbrella bundle.
    ///
    /// A bundle package is often a shell: `@nexus-aethra/dshell-bundle` ships a
    /// `package.json`, a `cordis.patch.yml` and a compiled `lib/` with no
    /// declaration this scanner can read, and its eleven sibling plugins are
    /// mounted by the patch rows. Without following that file the bundle is an
    /// empty node with no edges, which the panel then hides as isolated — so the
    /// one package a reader installed is the one they cannot see.
    pub inserts: Vec<String>,
}

/// One plugin's relation to one service.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceEdge {
    pub plugin: String,
    pub service: String,
}

/// A service name more than one activated plugin registers.
///
/// Not a conflict, and deliberately not named one: cordis resolves a service name
/// per isolation scope (`cordis/src/reflect.ts`), so a host implementation and a
/// browser implementation of the same name are the design — `sessions` is
/// provided by `dsh-session` in the host app and by
/// `dsh-api-session-controller/src/client/**` in the client app, and both are
/// correct. It is reported because this diagram merges the contexts, which turns
/// one name into a fan-out wider than anything the running tree has.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SharedService {
    pub service: String,
    /// The activated registrations of `service`, in name order.
    pub providers: Vec<String>,
    /// True when the registrations are one per context — a host implementation
    /// and a browser one. cordis resolves a service name per isolation scope, so
    /// that is not a conflict, and this diagram merges the two contexts, which is
    /// what makes the name look doubled. False means two registrations share a
    /// context, which the running tree really does have to arbitrate.
    #[serde(default)]
    pub per_context: bool,
}

/// A derived plugin-to-plugin edge: `from` requires `service`, which `to`
/// provides.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginLink {
    pub from: String,
    pub to: String,
    pub service: String,
    /// The two ends are in different cordis applications. cordis resolves a
    /// service name within an isolation scope, so this edge is not a load-order
    /// dependency — the requirement is met by whatever the other application
    /// mounts, which for a framework builtin is not in the graph at all. It is
    /// reported because the requirement is real, and kept out of the order
    /// because only within-context edges order anything.
    pub cross_context: bool,
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
    /// that the same name is implemented once per context.
    pub shared_services: Vec<SharedService>,
    /// Plugins a template's boxfile added, whose sources are not in its tree.
    ///
    /// A sealed template holds the base tree and a recipe: `ADD plugin` entries are
    /// installed when a container is created from it, so scanning the template
    /// cannot find them. They are listed here — and added as nodes — so that the
    /// one package the reader added is visible in the preview instead of silently
    /// absent from it.
    #[serde(default)]
    pub recipe_plugins: Vec<String>,
    pub diagnostics: Vec<String>,
    pub scanned_at: u64,
}

/// The launcher is not a package: `apps/cli` builds the root context and
/// provides a few services to the plugins it mounts. Nothing in the profile's
/// bundle list names it, so without a node of its own every plugin that injects
/// one of those services reads as waiting on nobody — `profileContext` was
/// reported missing for a container whose own startup audit reports nothing.
pub const LAUNCHER_NODE: &str = "dsh (launcher)";

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
    if let Some(harness) = &roots.harness {
        // Only the launcher's own sources: `apps/cli` is where the root context
        // is built, and scanning the rest of the app would attribute whatever it
        // provides in passing to the tree that is running.
        let launcher = harness.join("apps").join("cli").join("src");
        if launcher.is_dir() {
            candidates.push(Candidate {
                name: LAUNCHER_NODE.to_owned(),
                version: None,
                directory: launcher,
                display: "launcher".to_owned(),
            });
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
    let mut activated = activation(roots.profile.as_deref(), &unique, &mut diagnostics);
    // The launcher is the process: it is always loaded, and no bundle list names
    // it. Without this its services look like an installed-but-inactive provider.
    if let Some(names) = activated.as_mut() {
        if unique.contains_key(LAUNCHER_NODE) {
            names.insert(LAUNCHER_NODE.to_owned());
        }
    }

    let mut discovered: Vec<Discovered> = Vec::new();
    for candidate in unique.into_values() {
        let files = source_files(&candidate.directory, &mut diagnostics);
        // Where the package's own manifest says its browser entry's sources are.
        // `None` for a host-only package, and for one that declares a client
        // entry whose sources cannot be located — the latter is reported rather
        // than guessed at.
        let client_prefixes = client_half_prefixes(&candidate, &mut diagnostics);
        let inserts = bundle_inserts(&candidate, &mut diagnostics);
        let mut host = Scan::default();
        let mut client = Scan::default();
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
                    let belongs_to_client = client_prefixes
                        .iter()
                        .any(|prefix| file.starts_with(prefix));
                    scan_merge(if belongs_to_client { &mut client } else { &mut host }, next);
                }
                Err(error) => {
                    diagnostics.push(format!("cannot read {}: {error}", display_path(&file)))
                }
            }
        }
        let has_declarations = |scan: &Scan| {
            !scan.provides.is_empty() || !scan.requires.is_empty() || !scan.unresolved.is_empty()
        };
        let is_activated = activated
            .as_ref()
            .is_some_and(|names| names.contains(&candidate.name));
        // A half that declares nothing is not a node: many `dsh-client-ui-*`
        // packages ship an empty host body beside a real browser half, and giving
        // the stub its own node would be fourteen nodes of noise.
        if has_declarations(&host) || has_declarations(&client) || is_activated {
            discovered.push(Discovered {
                name: candidate.name.clone(),
                version: candidate.version.clone(),
                source: candidate.display,
                host,
                client: has_declarations(&client).then_some(client),
                inserts,
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

/// Where a package's browser half lives, as its own manifest declares it.
///
/// `dsh.client` is what makes a package dual-face, and `exports["./client"]` names
/// the browser entry. Two layouts answer to that:
///
/// * a package with a `src/` tree names a build output — `./lib/client.js` — whose
///   last component says which source directory holds the half, `src/client`;
/// * a published package has no `src/` to point at, so the same declaration
///   addresses the code itself: `lib/client.js` and anything under `lib/client/`.
///
/// Both reduce to path prefixes, which is what the caller partitions files by.
/// Reading only the first layout reported every published plugin as a package whose
/// "browser half is not at src/client" — 12 rows on one container, for packages that
/// ship no `src/` at all.
///
/// There can be two prefixes, and a package that ships both is why: `lib/client.js`
/// is the compiled half, and `lib/client/` beside it holds the chunks it imports.
/// Returning only the directory attributed the entry file to the *host* node —
/// `lib/client.js` does not start with `lib/client` — so the browser half's
/// `inject` list landed on the host, and every service only the browser context
/// provides was then reported as an installed-but-inactive provider.
fn client_half_prefixes(candidate: &Candidate, diagnostics: &mut Vec<String>) -> Vec<PathBuf> {
    let manifest = fs::read_to_string(candidate.directory.join("package.json")).ok();
    let Some(value) = manifest
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    else {
        return Vec::new();
    };
    if value.pointer("/dsh/client").is_none() {
        return Vec::new();
    }
    // The subpath is the literal key `./client`, and a JSON Pointer escapes `/`
    // as `~1`. `default` over `types`: the compiled entry names the half, while the
    // declaration file inside it is named `index`.
    let Some(entry) = value.pointer("/exports/.~1client") else {
        return Vec::new();
    };
    let target = ["default", "types"]
        .iter()
        .filter_map(|key| entry.get(key).and_then(serde_json::Value::as_str))
        .next()
        .unwrap_or_default();
    let entry = target.trim_start_matches("./");
    let stem = entry.rsplit_once('.').map_or(entry, |(head, _)| head);
    let has_source_tree = candidate.directory.join("src").is_dir();
    // The entry file is always part of the half. A published package addresses
    // the code itself (`lib/client.js`), and a package with a source tree may
    // address either its source (`src/client/index.ts`) or a build output whose
    // stem names the source directory (`lib/client.js` → `src/client`).
    let mut prefixes: Vec<PathBuf> = Vec::new();
    let entry_file = candidate.directory.join(entry);
    if entry_file.is_file() {
        prefixes.push(entry_file);
    }
    let (half_dir, expected) = if !has_source_tree {
        (candidate.directory.join(stem), stem.to_owned())
    } else if let Some(inside) = entry.strip_prefix("src/") {
        let parent = inside.rsplit_once('/').map(|(head, _)| head).unwrap_or_default();
        (
            candidate.directory.join("src").join(parent),
            format!("src/{parent}"),
        )
    } else {
        let segment = stem.rsplit('/').find(|part| !part.is_empty()).unwrap_or_default();
        (
            candidate.directory.join("src").join(segment),
            format!("src/{segment}"),
        )
    };
    if half_dir.is_dir() || half_dir.is_file() {
        prefixes.push(half_dir);
    }
    if !prefixes.is_empty() {
        return prefixes;
    }
    diagnostics.push(format!(
        "{}: declares dsh.client but its browser half is not at {expected}",
        candidate.name
    ));
    Vec::new()
}

/// The plugin names a bundle package's patch file inserts, in name order.
///
/// Only `name` is read: an umbrella's rows are `{ id, name }` pairs, and the `id`
/// is a patch-row address rather than a package. A patch that cannot be read
/// yields nothing, because a bundle with an unreadable patch is not evidence that
/// it inserts anything in particular.
fn bundle_inserts(candidate: &Candidate, diagnostics: &mut Vec<String>) -> Vec<String> {
    let Ok(manifest) = fs::read_to_string(candidate.directory.join("package.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest) else {
        return Vec::new();
    };
    let Some(patch) = value
        .pointer("/dsh/bundle/patch")
        .and_then(serde_json::Value::as_str)
    else {
        return Vec::new();
    };
    let path = candidate.directory.join(patch);
    let Ok(text) = fs::read_to_string(&path) else {
        diagnostics.push(format!(
            "{}: cannot read its bundle patch at {patch}",
            candidate.name
        ));
        return Vec::new();
    };
    let Ok(document) = serde_yaml::from_str::<serde_yaml::Value>(&text) else {
        diagnostics.push(format!(
            "{}: bundle patch {patch} is not YAML",
            candidate.name
        ));
        return Vec::new();
    };
    let Some(rows) = document.as_sequence() else {
        return Vec::new();
    };
    let mut names: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        let entries: Vec<&serde_yaml::Value> = match row.get("insert").and_then(|v| v.as_sequence()) {
            Some(inserted) => inserted.iter().collect(),
            None => vec![row],
        };
        for entry in entries {
            if let Some(name) = entry.get("name").and_then(|value| value.as_str()) {
                names.insert(name.to_owned());
            }
        }
    }
    names.into_iter().collect()
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
    is_build_dir(name)
        || matches!(name, "node_modules" | ".git" | ".cache")
        || matches!(name, "tests" | "test" | "__tests__")
}

/// Output directories that hold generated copies of a package's sources. They are
/// skipped only when the sources themselves are there to read instead.
fn is_build_dir(name: &str) -> bool {
    matches!(name, "dist" | "lib" | "build" | "coverage")
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
    let has_source_tree = package.join("src").is_dir();
    let start = if has_source_tree {
        package.join("src")
    } else {
        package.to_path_buf()
    };
    let mut files = Vec::new();
    let mut discovered = 0usize;
    // A package with no `src/` is a published artifact, and its `lib/` is not
    // build output — it is the only code there is. Skipping it by name made every
    // third-party plugin look like it declared nothing: `@nexus-aethra/dshell-commands`
    // carries `export const inject = [...]` and still arrived an isolated node,
    // which the panel then hid.
    collect_sources(&start, !has_source_tree, &mut files, &mut discovered);
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
fn collect_sources(
    directory: &Path,
    reads_build_output: bool,
    files: &mut Vec<PathBuf>,
    discovered: &mut usize,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let skipped = is_skipped_dir(&name) && !(reads_build_output && is_build_dir(&name));
            if skipped || name.starts_with('.') {
                continue;
            }
            collect_sources(&path, reads_build_output, files, discovered);
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
