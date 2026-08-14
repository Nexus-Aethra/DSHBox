//! DSH runtime DTOs and source repository constants.

use box_foundation::{is_safe_identifier, BoxResult};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
pub const DSH_REPOSITORY: &str = "https://github.com/deepseek-ai/deepseek-harness.git";
pub const DSH_TAGS_API: &str =
    "https://api.github.com/repos/deepseek-ai/deepseek-harness/tags?per_page=100";
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DshVersion {
    pub name: String,
    pub installed: bool,
}

pub fn version_directory(root: &str, version: &str) -> PathBuf {
    PathBuf::from(root)
        .join("runtimes")
        .join(version)
        .join("source")
}

pub fn installed_versions(root: &str) -> BoxResult<Vec<String>> {
    let directory = PathBuf::from(root).join("runtimes");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut versions = fs::read_dir(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            is_safe_identifier(name) && version_directory(root, name).join(".git").is_dir()
        })
        .collect::<Vec<_>>();
    versions.sort();
    Ok(versions)
}
