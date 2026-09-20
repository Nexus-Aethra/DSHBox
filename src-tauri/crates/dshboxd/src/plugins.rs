//! What plugins actually exist on this machine, and where they came from.
//!
//! The extension repository only knows what was explicitly imported into it. A
//! plugin that a template's boxfile installed, or that a container is running
//! with, never appears there — which reads as "I installed it and the plugin
//! list is empty". pnpm's lockfiles are the missing half: every template and
//! container keeps one, and it names every plugin with the version pnpm
//! resolved, including the ones a bundle pulled in transitively.

use box_extensions::ExtensionKind;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::sealed::{list_sealed_templates, template_root_by_name};
use crate::state::DaemonState;

#[derive(Default)]
struct Row {
    versions: BTreeSet<String>,
    owners: Vec<Value>,
    in_repository: bool,
}

fn owners_of<'a>(rows: &'a mut BTreeMap<String, Row>, name: &str) -> &'a mut Row {
    rows.entry(name.to_owned()).or_default()
}

/// Every plugin Box can see: the repository, plus each sealed template's and
/// each container's resolved package set.
pub(crate) fn list_installed_plugins(_state: &DaemonState, _request: &Value) -> Result<Value, String> {
    let root = box_foundation::read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // Refresh the index first: a template built or a container created since the
    // last scan must show up without waiting for the next daemon start. A failed
    // refresh is not fatal — the list below still reports what it can see.
    if let Err(error) = reconcile_plugin_index(Path::new(&root)) {
        tracing::warn!("plugin index refresh skipped: {error}");
    }
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();

    for entry in box_extensions::scan_repository(Path::new(&root)) {
        if entry.kind != ExtensionKind::Plugin {
            continue;
        }
        let row = owners_of(&mut rows, &entry.name);
        row.in_repository = true;
        if let Some(version) = &entry.version {
            row.versions.insert(version.clone());
        }
        row.owners.push(json!({
            "kind": "repository",
            "id": entry.id,
            "name": entry.name,
            "version": entry.version,
            "direct": true,
            "diagnostic": entry.diagnostic,
        }));
    }

    for template in list_sealed_templates()? {
        // A prepared base carries no plugin recipe; only a built template does.
        if !template.built {
            continue;
        }
        let Ok((profile, directory)) = template_root_by_name(&root, &template.name) else {
            continue;
        };
        for package in read_lock(&directory, &profile) {
            if !package.plugin {
                continue;
            }
            let row = owners_of(&mut rows, &package.name);
            row.versions.insert(package.version.clone());
            row.owners.push(json!({
                "kind": "template",
                "id": template.id,
                "name": template.name,
                "version": package.version,
                "direct": package.direct,
            }));
        }
    }

    let containers = box_containers::scan_containers(&root).unwrap_or_default();
    for container in containers.into_values() {
        for package in read_lock(Path::new(&container.directory), &container.profile) {
            if !package.plugin {
                continue;
            }
            let row = owners_of(&mut rows, &package.name);
            row.versions.insert(package.version.clone());
            row.owners.push(json!({
                "kind": "container",
                "id": container.id,
                "name": container.name,
                "version": package.version,
                "direct": package.direct,
            }));
        }
    }

    // Box gives pnpm a private store, so this answers "can this install
    // offline?". An unrecognised store is reported as unknown rather than as
    // "nothing cached".
    let cached = box_toolchains::cached_packages(Path::new(&root)).ok();
    let plugins: Vec<Value> = rows
        .into_iter()
        .map(|(name, row)| {
            let versions: Vec<String> = row.versions.into_iter().collect();
            let cached_versions: Option<Vec<String>> = cached.as_ref().map(|cached| {
                versions
                    .iter()
                    .filter(|version| cached.contains(&format!("{name}@{version}")))
                    .cloned()
                    .collect()
            });
            json!({
                "name": name,
                "versions": versions,
                "inRepository": row.in_repository,
                "owners": row.owners,
                "cachedVersions": cached_versions,
            })
        })
        .collect();
    Ok(json!({ "plugins": plugins }))
}

/// Make the index a mirror of what is installed: every plugin a sealed
/// template's or a container's lock resolves gets a derived row, so the user
/// never has to "import" something that is already on disk, and a derived row
/// nothing installs any more goes away again. Rows the user owns are left
/// alone. A failure here is never fatal — the scan is a convenience, not a
/// precondition.
pub(crate) fn reconcile_plugin_index(root: &Path) -> Result<usize, String> {
    let wanted = installed_plugin_versions(root);
    box_extensions::sync_derived_entries(root, ExtensionKind::Plugin, &wanted)
}

/// The plugins templates and containers actually resolved, one row per name —
/// the newest resolution wins, since that is the one still installed.
fn installed_plugin_versions(root: &Path) -> Vec<box_extensions::DerivedEntry> {
    let mut seen: BTreeMap<String, box_extensions::DerivedEntry> = BTreeMap::new();
    let mut record = |packages: Vec<box_plugin_graph::lockfile::LockedPackage>| {
        for package in packages {
            if package.plugin {
                seen.insert(package.name.clone(), derived_entry(&package));
            }
        }
    };
    for template in list_sealed_templates().unwrap_or_default() {
        if !template.built {
            continue;
        }
        let Ok((profile, directory)) = template_root_by_name(root.to_string_lossy().as_ref(), &template.name) else {
            continue;
        };
        record(read_lock(&directory, &profile));
    }
    for container in box_containers::scan_containers(root.to_string_lossy().as_ref()).unwrap_or_default().into_values() {
        record(read_lock(Path::new(&container.directory), &container.profile));
    }
    seen.into_values().collect()
}

/// What `pnpm add` would take to get this package again: a plain version is
/// better said as `name@version`, but a `file:`/git spec is the only form that
/// can be resolved at all.
fn derived_entry(package: &box_plugin_graph::lockfile::LockedPackage) -> box_extensions::DerivedEntry {
    let spec = match &package.specifier {
        Some(spec) if spec.contains(':') => spec.clone(),
        _ if package.version.contains(':') => package.version.clone(),
        _ => format!("{}@{}", package.name, package.version),
    };
    box_extensions::DerivedEntry {
        name: package.name.clone(),
        version: package.version.clone(),
        spec,
    }
}

/// A container or template keeps its pnpm resolution beside its profile.
fn read_lock(directory: &Path, profile: &str) -> Vec<box_plugin_graph::lockfile::LockedPackage> {
    std::fs::read_to_string(
        directory
            .join("profile")
            .join("profiles")
            .join(profile)
            .join("pnpm-lock.yaml"),
    )
    .ok()
    .and_then(|text| box_plugin_graph::lockfile::parse_lockfile(&text).ok())
    .unwrap_or_default()
}
