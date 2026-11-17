//! Reading pnpm's own resolution instead of guessing from a recipe.
//!
//! A sealed template keeps no `node_modules` — it keeps `pnpm-lock.yaml`, which
//! is where pnpm recorded exactly what it resolved: every package, its exact
//! version, and the edges between them. That file is the truth about what a
//! container will install, including the plugins a bundle pulled in that no
//! boxfile ever named, so the plugin inventory does not have to be inferred
//! from `ADD` lines.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedDependency {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    /// Named by the profile's own manifest (`importers['.']`).
    pub direct: bool,
    /// The specifier the profile asked for, when it is a direct dependency.
    #[serde(default)]
    pub specifier: Option<String>,
    /// A DSH/cordis plugin rather than a plain library: it is a direct
    /// dependency, or it declares a cordis/dsh peer.
    pub plugin: bool,
    pub dependencies: Vec<LockedDependency>,
}

/// Parse a pnpm lockfile (v6 or v9 shapes) into the resolved package set.
///
/// v6 keys packages as `/name@version`, v9 as `name@version`, and both suffix a
/// version with its peer set (`1.2.3(peer@4.5.6)`) — the suffix is not part of
/// the version and is dropped. `snapshots` (v9) holds the edges; older locks
/// keep them on the package entry itself.
pub fn parse_lockfile(text: &str) -> Result<Vec<LockedPackage>, String> {
    let root: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|error| format!("invalid pnpm lockfile: {error}"))?;

    let direct: Vec<(String, String, Option<String>)> = root
        .get("importers")
        .and_then(|importers| importers.get("."))
        .and_then(|importer| importer.get("dependencies"))
        .and_then(|dependencies| dependencies.as_mapping())
        .map(|mapping| {
            mapping
                .iter()
                .filter_map(|(name, entry)| {
                    let name = name.as_str()?.to_owned();
                    let version = entry.get("version").and_then(|v| v.as_str()).map(clean_version);
                    let specifier = entry
                        .get("specifier")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    // A link/workspace spec has no registry version.
                    Some((name, version.unwrap_or_else(|| "link".to_owned()), specifier))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut packages: Vec<LockedPackage> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    // `snapshots` keys the same package the same way, so a key already resolved
    // against `packages` must not be re-read as a version.
    let mut resolved: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for key in ["packages", "snapshots"] {
        let Some(entries) = root.get(key).and_then(|section| section.as_mapping()) else {
            continue;
        };
        for (key, entry) in entries {
            let Some(raw) = key.as_str() else { continue };
            let Some((name, key_version)) = split_key(raw) else {
                continue;
            };
            // Keyed by name and the version slot, not the raw key: `snapshots`
            // repeats a package under a peer-suffixed key.
            let slot = format!("{name}@{key_version}");
            let version = match resolved.get(&slot) {
                Some(version) => version.clone(),
                None => {
                    // pnpm keys a `file:`/`link:`/git dependency by its spec, and
                    // only the entry itself carries the version it declares.
                    let version = if is_protocol_spec(&key_version) {
                        entry
                            .get("version")
                            .and_then(|value| value.as_str())
                            .map(clean_version)
                            .unwrap_or(key_version)
                    } else {
                        key_version
                    };
                    resolved.insert(slot, version.clone());
                    version
                }
            };
            let is_direct = direct.iter().find(|(direct_name, _, _)| direct_name == &name);
            let plugin = is_direct.is_some() || declares_dsh_peer(entry);
            let mut dependencies: Vec<LockedDependency> = entry
                .get("dependencies")
                .and_then(|deps| deps.as_mapping())
                .map(|mapping| edges(mapping))
                .unwrap_or_default();
            dependencies.extend(
                entry
                    .get("optionalDependencies")
                    .and_then(|deps| deps.as_mapping())
                    .map(|mapping| edges(mapping))
                    .unwrap_or_default(),
            );
            dependencies.sort_by(|left, right| left.name.cmp(&right.name));

            if let Some(existing) = packages
                .iter_mut()
                .find(|package| package.name == name && package.version == version)
            {
                // `packages` has the metadata, `snapshots` the edges.
                existing.dependencies.extend(dependencies);
                existing.dependencies.sort_by(|left, right| left.name.cmp(&right.name));
                existing.dependencies.dedup();
                continue;
            }
            if !seen.insert(format!("{name}@{version}")) {
                continue;
            }
            packages.push(LockedPackage {
                name: name.clone(),
                version: version.clone(),
                direct: is_direct.is_some(),
                specifier: is_direct.and_then(|(_, _, specifier)| specifier.clone()),
                plugin,
                dependencies,
            });
        }
    }

    // A direct dependency whose peer-suffixed version never matched a package
    // entry still belongs in the list: it is what the profile asked for.
    for (name, version, specifier) in direct {
        if packages.iter().any(|package| package.name == name) {
            continue;
        }
        packages.push(LockedPackage {
            name,
            version,
            direct: true,
            specifier,
            plugin: true,
            dependencies: Vec::new(),
        });
    }

    packages.sort_by(|left, right| left.name.cmp(&right.name).then(left.version.cmp(&right.version)));
    Ok(packages)
}

fn edges(mapping: &serde_yaml::Mapping) -> Vec<LockedDependency> {
    mapping
        .iter()
        .filter_map(|(name, version)| {
            Some(LockedDependency {
                name: name.as_str()?.to_owned(),
                version: version.as_str().map(clean_version).unwrap_or_else(|| "link".to_owned()),
            })
        })
        .collect()
}

/// `@scope/name@1.2.3(peer@4.5.6)` → `@scope/name` + `1.2.3`.
fn split_key(key: &str) -> Option<(String, String)> {
    let key = key.strip_prefix('/').unwrap_or(key);
    let key = key.split('(').next().unwrap_or(key);
    let (name, version) = key.rsplit_once('@')?;
    if name.is_empty() || version.is_empty() {
        return None;
    }
    Some((name.to_owned(), version.to_owned()))
}

/// Drop the peer suffix pnpm appends to a version.
fn clean_version(version: &str) -> String {
    version.split('(').next().unwrap_or(version).to_owned()
}

/// A version slot holding something other than a version: `file:../x.tgz`,
/// `link:../local-plugin`, `https://…`, `github:owner/repo`.
fn is_protocol_spec(version: &str) -> bool {
    version.contains(':')
}

/// A plugin declares the framework it plugs into: a cordis or dsh peer. Plain
/// libraries (`zod`) declare none, which is what keeps the inventory readable.
fn declares_dsh_peer(entry: &serde_yaml::Value) -> bool {
    let Some(peers) = entry.get("peerDependencies").and_then(|peers| peers.as_mapping()) else {
        return false;
    };
    peers.keys().filter_map(|key| key.as_str()).any(|name| {
        name.contains("cordis") || name.starts_with("@deepseek-ai/dsh") || name.contains("dshell")
    })
}

/// The plugins a container or template will load, with the versions pnpm
/// actually resolved.
pub fn plugins(packages: &[LockedPackage]) -> Vec<&LockedPackage> {
    packages.iter().filter(|package| package.plugin).collect()
}

/// How many of the resolved packages are plugins, for a one-line diagnostic.
pub fn summary(packages: &[LockedPackage]) -> String {
    let plugins = packages.iter().filter(|package| package.plugin).count();
    let direct = packages.iter().filter(|package| package.direct).count();
    format!(
        "pnpm resolved {} package(s): {direct} named by the profile, {plugins} plugin(s) in total",
        packages.len()
    )
}

#[cfg(test)]
#[path = "lockfile_tests.rs"]
mod tests;
