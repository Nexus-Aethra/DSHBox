//! Read-only discovery of per-container DSH profiles, plugins, and skills.

use box_containers::DshContainer;
use box_foundation::now_seconds;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionPlugin {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub path: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileExtensions {
    pub name: String,
    pub plugins: Vec<ExtensionPlugin>,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSkill {
    pub name: String,
    pub description: Option<String>,
    pub path: String,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainerExtensions {
    pub container_id: String,
    pub profiles: Vec<ProfileExtensions>,
    pub skills: Vec<ContainerSkill>,
    pub diagnostics: Vec<String>,
    pub scanned_at: u64,
}

/// Scans only data owned by one container; project and runtime skill roots are excluded.
pub fn scan_container_extensions(container: &DshContainer) -> ContainerExtensions {
    let profile_root = PathBuf::from(&container.directory).join("profile");
    let mut details = ContainerExtensions {
        container_id: container.id.clone(),
        profiles: Vec::new(),
        skills: Vec::new(),
        diagnostics: Vec::new(),
        scanned_at: now_seconds(),
    };
    details.profiles = scan_profiles(&profile_root, &mut details.diagnostics);
    details.skills = scan_skills(&profile_root.join("skills"), &mut details.diagnostics);
    details
}

fn scan_profiles(root: &Path, diagnostics: &mut Vec<String>) -> Vec<ProfileExtensions> {
    let directory = root.join("profiles");
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut profiles = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "node_modules" {
                return None;
            }
            Some(scan_profile(
                &name,
                &entry.path(),
                &directory.join("node_modules"),
            ))
        })
        .collect::<Vec<_>>();
    profiles.sort_by(|left, right| left.name.cmp(&right.name));
    if profiles.is_empty() && directory.exists() {
        diagnostics.push("no DSH profiles found".to_owned());
    }
    profiles
}

fn scan_profile(name: &str, directory: &Path, shared_modules: &Path) -> ProfileExtensions {
    let manifest = directory.join("package.json");
    let mut result = ProfileExtensions {
        name: name.to_owned(),
        plugins: Vec::new(),
        diagnostics: Vec::new(),
    };
    let content = match fs::read_to_string(&manifest) {
        Ok(content) => content,
        Err(error) => {
            result
                .diagnostics
                .push(format!("cannot read {}: {error}", manifest.display()));
            return result;
        }
    };
    let value: Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(error) => {
            result
                .diagnostics
                .push(format!("cannot parse {}: {error}", manifest.display()));
            return result;
        }
    };
    let bundles = value
        .pointer("/dsh/profile/bundles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for bundle in bundles {
        let Some(package) = bundle.as_str() else {
            result
                .diagnostics
                .push("profile bundle is not a package name".to_owned());
            continue;
        };
        result
            .plugins
            .push(read_plugin(package, directory, shared_modules));
    }
    result
}

fn read_plugin(package: &str, profile: &Path, shared_modules: &Path) -> ExtensionPlugin {
    let local = profile
        .join("node_modules")
        .join(package)
        .join("package.json");
    let shared = shared_modules.join(package).join("package.json");
    let manifest = [local, shared].into_iter().find(|path| path.is_file());
    let Some(manifest) = manifest else {
        return ExtensionPlugin {
            name: package.to_owned(),
            version: None,
            description: None,
            path: None,
            diagnostic: Some("package is declared by the profile but is not installed".to_owned()),
        };
    };
    let content = match fs::read_to_string(&manifest) {
        Ok(content) => content,
        Err(error) => {
            return ExtensionPlugin {
                name: package.to_owned(),
                version: None,
                description: None,
                path: Some(manifest.to_string_lossy().into_owned()),
                diagnostic: Some(format!("cannot read package metadata: {error}")),
            }
        }
    };
    match serde_json::from_str::<Value>(&content) {
        Ok(value) => ExtensionPlugin {
            name: value["name"].as_str().unwrap_or(package).to_owned(),
            version: value["version"].as_str().map(str::to_owned),
            description: value["description"].as_str().map(str::to_owned),
            path: Some(
                manifest
                    .parent()
                    .unwrap_or(&manifest)
                    .to_string_lossy()
                    .into_owned(),
            ),
            diagnostic: None,
        },
        Err(error) => ExtensionPlugin {
            name: package.to_owned(),
            version: None,
            description: None,
            path: Some(manifest.to_string_lossy().into_owned()),
            diagnostic: Some(format!("cannot parse package metadata: {error}")),
        },
    }
}

fn scan_skills(root: &Path, diagnostics: &mut Vec<String>) -> Vec<ContainerSkill> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let candidate = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
            path
        } else {
            continue;
        };
        if candidate.file_name().and_then(|value| value.to_str()) != Some("SKILL.md")
            && candidate.parent() != Some(root)
        {
            continue;
        }
        skills.push(read_skill(&candidate));
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    if !root.exists() {
        return skills;
    }
    if skills.is_empty() && root.is_dir() {
        diagnostics.push("no container skills found".to_owned());
    }
    skills
}

fn read_skill(path: &Path) -> ContainerSkill {
    let fallback = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("unnamed-skill")
        .to_owned();
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            return ContainerSkill {
                name: fallback,
                description: None,
                path: path.to_string_lossy().into_owned(),
                diagnostic: Some(format!("cannot read skill: {error}")),
            }
        }
    };
    let (name, description, diagnostic) =
        parse_frontmatter(&content).unwrap_or_else(|error| (fallback, None, Some(error)));
    ContainerSkill {
        name,
        description,
        path: path.to_string_lossy().into_owned(),
        diagnostic,
    }
}

fn parse_frontmatter(content: &str) -> Result<(String, Option<String>, Option<String>), String> {
    let Some(body) = content.strip_prefix("---\n") else {
        return Err("missing YAML frontmatter".to_owned());
    };
    let Some((frontmatter, _)) = body.split_once("\n---") else {
        return Err("unterminated YAML frontmatter".to_owned());
    };
    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']).to_owned();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            _ => {}
        }
    }
    Ok((
        name.ok_or("skill frontmatter has no name")?,
        description,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::Path};

    fn container(root: &Path) -> DshContainer {
        DshContainer {
            id: "one".to_owned(),
            name: "One".to_owned(),
            version: "latest".to_owned(),
            profile: "web".to_owned(),
            directory: root.to_string_lossy().into_owned(),
            status: "stopped".to_owned(),
        }
    }

    #[test]
    fn scans_profiles_scoped_plugins_and_container_skills() {
        let root = std::env::temp_dir().join(format!("dshbox-extension-test-{}", now_seconds()));
        fs::create_dir_all(root.join("profile/profiles/web/node_modules/@scope/plugin")).unwrap();
        fs::create_dir_all(root.join("profile/skills/demo")).unwrap();
        fs::write(
            root.join("profile/profiles/web/package.json"),
            r#"{"dsh":{"profile":{"bundles":["@scope/plugin","missing"]}}}"#,
        )
        .unwrap();
        fs::write(
            root.join("profile/profiles/web/node_modules/@scope/plugin/package.json"),
            r#"{"name":"@scope/plugin","version":"1.2.3","description":"Plugin"}"#,
        )
        .unwrap();
        fs::write(
            root.join("profile/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\n",
        )
        .unwrap();
        let found = scan_container_extensions(&container(&root));
        assert_eq!(
            found.profiles[0].plugins[0].version.as_deref(),
            Some("1.2.3")
        );
        assert!(found.profiles[0].plugins[1].diagnostic.is_some());
        assert_eq!(found.skills[0].name, "demo");
        let _ = fs::remove_dir_all(root);
    }
}
