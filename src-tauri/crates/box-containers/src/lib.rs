//! Persistent DSH container metadata independent from desktop windows.

use box_foundation::BoxResult;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf};
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DshContainer {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default = "default_profile")]
    pub profile: String,
    pub directory: String,
    pub status: String,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDshContainerRequest {
    pub name: String,
    pub version: String,
    pub profile: String,
}

fn default_profile() -> String {
    "web".to_owned()
}

pub fn container_directory(root: &str, id: &str) -> PathBuf {
    PathBuf::from(root).join("instances").join(id)
}

pub fn scan_containers(root: &str) -> BoxResult<BTreeMap<String, DshContainer>> {
    let directory = PathBuf::from(root).join("instances");
    if !directory.exists() {
        return Ok(BTreeMap::new());
    }
    let mut containers = BTreeMap::new();
    for entry in fs::read_dir(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
    {
        let metadata = match fs::read_to_string(entry.path().join("container.json")) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let value: serde_json::Value = match serde_json::from_str(&metadata) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let (Some(id), Some(version)) = (value["id"].as_str(), value["version"].as_str()) else {
            continue;
        };
        containers.insert(
            id.to_owned(),
            DshContainer {
                id: id.to_owned(),
                name: value["name"].as_str().unwrap_or(id).to_owned(),
                version: version.to_owned(),
                profile: value["profile"].as_str().unwrap_or("web").to_owned(),
                directory: entry.path().to_string_lossy().into_owned(),
                status: "stopped".to_owned(),
            },
        );
    }
    Ok(containers)
}
