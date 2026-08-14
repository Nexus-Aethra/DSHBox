//! Toolchain identifiers and feature-level dependency contracts.

use box_foundation::suppress_console_window;
use serde::{Deserialize, Serialize};
use std::{env, path::PathBuf, process::Command};

pub const TOOLCHAIN_IDS: [&str; 3] = ["node", "npm", "pnpm"];
pub fn is_known_toolchain(id: &str) -> bool {
    TOOLCHAIN_IDS.contains(&id)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolchainStatus {
    pub id: String,
    pub name: String,
    pub system_version: Option<String>,
    pub managed_version: Option<String>,
}
pub trait ToolchainResolver: Send + Sync {
    fn resolve(&self, id: &str) -> Result<std::path::PathBuf, String>;
}

pub fn binary_names(id: &str) -> Vec<String> {
    if cfg!(target_os = "windows") && matches!(id, "npm" | "pnpm") {
        vec![format!("{id}.cmd"), format!("{id}.exe")]
    } else {
        vec![format!("{id}{}", env::consts::EXE_SUFFIX)]
    }
}

pub fn find_system_binary(id: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for directory in env::split_paths(&paths) {
        for binary in binary_names(id) {
            let candidate = directory.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn command_version(binary: PathBuf) -> Option<String> {
    let mut command = Command::new(binary);
    suppress_console_window(&mut command);
    command
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| {
            output
                .lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn git_is_not_a_box_toolchain() {
        assert!(!is_known_toolchain("git"));
    }
}
