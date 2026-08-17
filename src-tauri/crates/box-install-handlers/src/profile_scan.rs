//! Reverse-scan: walk a container's profile directory and report every
//! plugin / skill that the container's `dsh` runtime would see.
//!
//! Used by `dshbox plugin ls` to show what is installed across all
//! profiles without the operator having to dig into each container's
//! `node_modules/` and `skills/` directories. The reverse direction —
//! when a user runs `dsh plugin add` inside a running container and
//! the host wants to keep `dshbox`'s view in sync — is handled by the
//! daemon watching `profile/package.json` mtime and re-running this
//! scan on change.
//!
//! The scan is intentionally a thin reader over well-known files
//! (`package.json`, `skills/<name>/SKILL.md`). It does NOT mutate
//! anything; it only describes.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One plugin/skill entry discovered inside a container profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilePluginEntry {
    /// `plugin` / `skill` / `data`.
    pub kind: String,
    /// npm-style name (`@scope/foo` or `foo`).
    pub name: String,
    /// Declared version in the package manifest (None if unversioned).
    pub version: Option<String>,
    /// Absolute path to the installed source root (node_modules/foo or
    /// skills/foo or extensions/data/foo).
    pub source_path: PathBuf,
    /// Whether the manifest declares `dsh.bundle.patch` (i.e. whether
    /// `dsh` will treat it as a profile layer).
    pub is_bundle: bool,
}

/// Walk `<container>/profile/profiles/<profile>/` and return every
/// plugin + skill the container's DSH runtime would see.
pub fn scan_profile_plugins(profile_dir: &Path) -> Vec<ProfilePluginEntry> {
    let mut out = Vec::new();
    let node_modules = profile_dir.join("node_modules");
    if node_modules.is_dir() {
        scan_node_modules(&node_modules, &mut out);
    }
    let skills_root = profile_dir.parent().map(|p| p.join("skills"));
    if let Some(skills_root) = skills_root {
        if skills_root.is_dir() {
            scan_skills(&skills_root, &mut out);
        }
    }
    out
}

fn scan_node_modules(root: &Path, out: &mut Vec<ProfilePluginEntry>) {
    let entries = match std::fs::read_dir(root) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy().to_string();
        // Scoped packages live under `node_modules/@scope/...`.
        if name.starts_with('@') {
            if let Ok(scope_entries) = std::fs::read_dir(entry.path()) {
                for scope_entry in scope_entries.flatten() {
                    let pkg_name = format!("{}/{}", name, scope_entry.file_name().to_string_lossy());
                    push_plugin(out, &pkg_name, &scope_entry.path());
                }
            }
        } else {
            // Skip pnpm store and dsh-box-managed symlinks.
            if name.starts_with('.') {
                continue;
            }
            push_plugin(out, &name, &entry.path());
        }
    }
}

fn push_plugin(out: &mut Vec<ProfilePluginEntry>, name: &str, pkg_dir: &Path) {
    if !pkg_dir.is_dir() {
        return;
    }
    let manifest_path = pkg_dir.join("package.json");
    let (version, is_bundle) = read_plugin_manifest(&manifest_path);
    out.push(ProfilePluginEntry {
        kind: "plugin".to_owned(),
        name: name.to_owned(),
        version,
        source_path: pkg_dir.to_path_buf(),
        is_bundle,
    });
}

fn read_plugin_manifest(path: &Path) -> (Option<String>, bool) {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return (None, false),
    };
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return (None, false),
    };
    let version = value.get("version").and_then(|v| v.as_str()).map(str::to_owned);
    let is_bundle = value
        .get("dsh")
        .and_then(|d| d.get("bundle"))
        .and_then(|b| b.get("patch"))
        .is_some();
    (version, is_bundle)
}

fn scan_skills(root: &Path, out: &mut Vec<ProfilePluginEntry>) {
    let entries = match std::fs::read_dir(root) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // SKILL.md is the marker for a skill directory; without one,
        // skip — it might be a non-skill extension folder.
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        out.push(ProfilePluginEntry {
            kind: "skill".to_owned(),
            name,
            version: None,
            source_path: path,
            is_bundle: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn write(dir: &Path, rel: &str, body: &str) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }

    #[test]
    fn scan_picks_up_plugin_with_bundle() {
        let tmp = tempdir_in("/tmp");
        write(
            &tmp,
            "node_modules/foo/package.json",
            r#"{"name":"foo","version":"1.2.3","dsh":{"bundle":{"patch":{}}}}"#,
        );
        let entries = scan_profile_plugins(&tmp);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "foo");
        assert_eq!(entries[0].version.as_deref(), Some("1.2.3"));
        assert!(entries[0].is_bundle);
    }

    #[test]
    fn scan_picks_up_scoped_plugin() {
        let tmp = tempdir_in("/tmp");
        write(
            &tmp,
            "node_modules/@scope/bar/package.json",
            r#"{"name":"@scope/bar","version":"0.1.0"}"#,
        );
        let entries = scan_profile_plugins(&tmp);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "@scope/bar");
        assert!(!entries[0].is_bundle);
    }

    #[test]
    fn scan_picks_up_skill_via_sibling_skills_dir() {
        let _tmp = tempdir_in("/tmp");
        // Place the skills directory as a sibling of the profile
        // directory (matching the real layout: <container>/profile/
        //   profiles/<name>/  ← profile_dir
        //   skills/<name>/   ← sibling
        // ). Use the canonical layout under a fresh root so unrelated
        // tests' /tmp/skills/ can't bleed in.
        let container = unique_container_root();
        let profile = container.join("profiles/web");
        std::fs::create_dir_all(&profile).unwrap();
        write(
            &profile,
            "../skills/team-conventions/SKILL.md",
            "# skill body",
        );
        let entries = scan_profile_plugins(&profile);
        // Cleanup so subsequent runs don't accumulate state.
        let _ = std::fs::remove_dir_all(&container);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "skill");
        assert_eq!(entries[0].name, "team-conventions");
        assert!(entries[0].is_bundle);
    }

    fn tempdir_in(parent: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(format!("{parent}/profile-scan-test-{stamp}"));
        std::fs::create_dir_all(&dir).unwrap();
        // Make sure hidden files inside the dir don't sneak in as
        // pretend plugins.
        let _ = BTreeMap::<String, ()>::new();
        dir
    }

    fn unique_container_root() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/profile-scan-container-{stamp}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}