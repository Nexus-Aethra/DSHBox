//! Filesystem, configuration, and validation primitives shared by DSH Box features.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub type BoxResult<T> = Result<T, String>;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoxConfig {
    pub runtime_directory: Option<String>,
    pub selected_dsh_version: Option<String>,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub toolchain_sources: BTreeMap<String, String>,
}

fn default_language() -> String {
    "en".to_owned()
}

impl Default for BoxConfig {
    fn default() -> Self {
        Self {
            runtime_directory: None,
            selected_dsh_version: None,
            language: default_language(),
            toolchain_sources: BTreeMap::new(),
        }
    }
}

/// Canonical paths for user configuration and runtime-managed data.
#[derive(Debug, Clone)]
pub struct BoxPaths {
    pub config: PathBuf,
    pub runtime: Option<PathBuf>,
}

impl BoxPaths {
    pub fn from_config(config: &BoxConfig) -> BoxResult<Self> {
        Ok(Self {
            config: config_path()?,
            runtime: config.runtime_directory.as_ref().map(PathBuf::from),
        })
    }
    pub fn tasks_state(&self) -> BoxResult<PathBuf> {
        Ok(self
            .runtime
            .as_ref()
            .ok_or("DSH Box storage is not configured")?
            .join("state/tasks.json"))
    }
    pub fn task_log(&self, id: &str) -> BoxResult<PathBuf> {
        Ok(self
            .runtime
            .as_ref()
            .ok_or("DSH Box storage is not configured")?
            .join("logs/tasks")
            .join(format!("{id}.log")))
    }
}

pub fn config_path() -> BoxResult<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or("cannot determine home directory")?
        .join(".dsh-box/config.json"))
}
pub fn read_config() -> BoxResult<BoxConfig> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(BoxConfig::default());
    }
    let mut config: BoxConfig = serde_json::from_str(
        &fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    config.toolchain_sources.remove("git");
    Ok(config)
}
pub fn write_config(config: &BoxConfig) -> BoxResult<()> {
    let path = config_path()?;
    fs::create_dir_all(path.parent().ok_or("configuration path has no parent")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        path,
        serde_json::to_string_pretty(config).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}
pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn is_safe_identifier(value: &str) -> bool {
    value == "latest"
        || (!value.is_empty()
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
            }))
}
