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
mod tests {
    use super::*;

    /// Shapes taken from a real sealed template's lock: a plain library with no
    /// peers, a plugin with dsh/cordis peers, and a direct dependency whose
    /// version carries pnpm's peer suffix.
    const FIXTURE: &str = r#"
lockfileVersion: '9.0'

importers:

  .:
    dependencies:
      '@deepseek-ai/dsh-ssh':
        specifier: 0.1.6-alpha.1
        version: 0.1.6-alpha.1
      '@nexus-aethra/dshell-bundle':
        specifier: 0.1.5
        version: 0.1.5(@deepseek-ai/dsh-ssh@0.1.6-alpha.1)(zod@4.4.3)

packages:

  '@deepseek-ai/dsh-ssh@0.1.6-alpha.1':
    resolution: {integrity: sha512-aaa}
    peerDependencies:
      '@deepseek-ai/cordis': ^4.0.2
      '@deepseek-ai/dsh-fs': ^0.1.6-alpha.1
  '@nexus-aethra/dshell-conversation@0.1.3':
    resolution: {integrity: sha512-bbb}
    peerDependencies:
      '@deepseek-ai/cordis': ^4.0.2
  zod@4.4.3:
    resolution: {integrity: sha512-ccc}

snapshots:

  '@deepseek-ai/dsh-ssh@0.1.6-alpha.1':
    dependencies:
      '@deepseek-ai/schemastery': 3.18.2
      zod: 4.4.3
  '@nexus-aethra/dshell-bundle@0.1.5':
    dependencies:
      '@nexus-aethra/dshell-conversation': 0.1.3
  '@nexus-aethra/dshell-conversation@0.1.3': {}
  zod@4.4.3: {}
"#;

    #[test]
    fn resolves_the_plugins_and_their_versions() {
        let packages = parse_lockfile(FIXTURE).unwrap();
        let ssh = packages.iter().find(|package| package.name == "@deepseek-ai/dsh-ssh").unwrap();
        assert_eq!(ssh.version, "0.1.6-alpha.1");
        assert!(ssh.direct);
        assert!(ssh.plugin, "a dsh peer makes it a plugin");
        assert_eq!(ssh.specifier.as_deref(), Some("0.1.6-alpha.1"));

        // pnpm's peer suffix is not part of the version.
        let bundle = packages
            .iter()
            .find(|package| package.name == "@nexus-aethra/dshell-bundle")
            .unwrap();
        assert_eq!(bundle.version, "0.1.5");
        assert_eq!(bundle.dependencies.len(), 1);
        assert_eq!(bundle.dependencies[0].name, "@nexus-aethra/dshell-conversation");

        // A plugin nobody's boxfile named, pulled in transitively, is still a
        // plugin: that is the point of reading the lock.
        let conversation = packages
            .iter()
            .find(|package| package.name == "@nexus-aethra/dshell-conversation")
            .unwrap();
        assert!(!conversation.direct);
        assert!(conversation.plugin);

        // A plain library stays a library.
        let zod = packages.iter().find(|package| package.name == "zod").unwrap();
        assert!(!zod.plugin);
        assert!(!zod.direct);

        assert_eq!(plugins(&packages).len(), 3);
        assert!(summary(&packages).contains("4 package(s)"));
    }

    #[test]
    fn lockfile_v6_keys_and_links_are_understood() {
        let v6 = r#"
lockfileVersion: '6.0'
importers:
  .:
    dependencies:
      '@scope/plugin':
        specifier: 1.0.0
        version: 1.0.0
      local-plugin:
        specifier: link:../local-plugin
        version: link:../local-plugin
packages:
  /@scope/plugin@1.0.0:
    resolution: {integrity: sha512-ddd}
    dependencies:
      zod: 4.4.3
  /zod@4.4.3:
    resolution: {integrity: sha512-eee}
"#;
        let packages = parse_lockfile(v6).unwrap();
        let plugin = packages.iter().find(|package| package.name == "@scope/plugin").unwrap();
        assert_eq!(plugin.version, "1.0.0");
        assert_eq!(plugin.dependencies[0].name, "zod");
        // A linked local package has no registry version, and must still show up
        // as a direct dependency rather than vanishing.
        let linked = packages.iter().find(|package| package.name == "local-plugin").unwrap();
        assert!(linked.direct);
        // The raw spec is kept: `link:../local-plugin` says *where* it comes
        // from, which is the useful part for something pnpm never resolved.
        assert_eq!(linked.version, "link:../local-plugin");
    }

    /// A `file:` dependency is keyed by its spec, and the entry under that key
    /// carries the version the package declares. Reporting the spec as the
    /// version would put a relative path where a version belongs.
    #[test]
    fn a_file_dependency_reports_the_version_it_declares() {
        let lock = r#"
lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      '@scope/plugin':
        specifier: file:/tmp/pack/plugin-0.1.5.tgz
        version: file:../../../../tmp/pack/plugin-0.1.5.tgz
packages:
  '@scope/plugin@file:../../../../tmp/pack/plugin-0.1.5.tgz':
    resolution: {integrity: sha512-fff, tarball: file:../../../../tmp/pack/plugin-0.1.5.tgz}
    version: 0.1.5
    peerDependencies:
      '@deepseek-ai/cordis': ^4.0.2

snapshots:
  '@scope/plugin@file:../../../../tmp/pack/plugin-0.1.5.tgz(zod@4.4.3)':
    dependencies:
      zod: 4.4.3
"#;
        let packages = parse_lockfile(lock).unwrap();
        let plugin = packages.iter().find(|package| package.name == "@scope/plugin").unwrap();
        assert_eq!(plugin.version, "0.1.5");
        assert!(plugin.direct && plugin.plugin);
        assert_eq!(plugin.specifier.as_deref(), Some("file:/tmp/pack/plugin-0.1.5.tgz"));
        // The `snapshots` entry for the same key carries the edges, not a
        // second copy of the package.
        assert_eq!(packages.len(), 1);
        assert_eq!(plugin.dependencies.len(), 1);
    }
}
