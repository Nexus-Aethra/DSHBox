//! DSH version management for the daemon: catalog refresh, install, and
//! uninstall. Mirrors the desktop's `versions.rs` + `commands/versions.rs`
//! without any Tauri dependency.

use box_dsh_versions::{
    installed_versions, parse_template_ref, pull_template, upgrade_legacy_harness,
    version_directory, HarnessUpgradeReport, DSH_TAGS_API,
};
use box_foundation::{mirror_url, read_config, write_config};
use serde::Deserialize;
use std::{fs, path::PathBuf, time::Duration};

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    box_foundation::is_safe_identifier(version)
}

#[derive(Deserialize)]
struct GitHubTag {
    name: String,
}

pub(crate) fn dsh_catalog_path(root: &str) -> PathBuf {
    PathBuf::from(root).join("state/dsh-catalog.json")
}

pub(crate) fn read_dsh_catalog(root: &str) -> Vec<String> {
    fs::read_to_string(dsh_catalog_path(root))
        .ok()
        .and_then(|source| serde_json::from_str::<Vec<String>>(&source).ok())
        .unwrap_or_default()
}

pub(crate) fn fetch_dsh_tags() -> Result<Vec<String>, String> {
    let config = read_config()?;
    let endpoint = mirror_url(DSH_TAGS_API, config.github_mirror.as_deref());
    let client = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box/0.1")
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| format!("cannot create GitHub client: {error}"))?;
    let response = client
        .get(&endpoint)
        .send()
        .map_err(|error| format!("cannot reach GitHub: {error}"))?;
    let tags: Vec<GitHubTag> = response
        .error_for_status()
        .map_err(|error| format!("GitHub tags request failed: {error}"))?
        .json()
        .map_err(|error| format!("cannot parse GitHub tags: {error}"))?;
    Ok(tags
        .into_iter()
        .map(|tag| tag.name)
        .filter(|name| is_safe_version_name(name))
        .collect())
}

/// How long a fetched version catalog stays valid before the GitHub API is
/// called again (mirrors the desktop's TTL).
pub(crate) const DSH_CATALOG_TTL_SECONDS: u64 = 600;

pub(crate) fn refresh_dsh_catalog() -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let path = dsh_catalog_path(&root);
    // Reuse a recent catalog instead of hitting the network again.
    if let Ok(metadata) = fs::metadata(&path) {
        if let Ok(modified) = metadata.modified() {
            if let Ok(age) = modified.elapsed() {
                if age.as_secs() < DSH_CATALOG_TTL_SECONDS {
                    return Ok(());
                }
            }
        }
    }
    let tags = fetch_dsh_tags()?;
    fs::create_dir_all(path.parent().ok_or("invalid DSH catalog path")?)
        .map_err(|error| error.to_string())?;
    fs::write(
        path,
        serde_json::to_string(&tags).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

/// Pull a template by reference (e.g. `github.com/deepseek-ai/deepseek-harness:latest`)
/// and select it as the active version. The `cancelled` probe lets task
/// workers abort a long clone.
pub(crate) fn pull_template_with_cancel(
    ref_value: String,
    cancelled: impl Fn() -> bool + Send + 'static,
) -> Result<(), String> {
    parse_template_ref(&ref_value).map_err(|error| format!("invalid template reference: {error}"))?;
    let root = read_config()?
        .runtime_directory
        .clone()
        .ok_or("DSH Box storage is not configured")?;
    let version = pull_template(&root, &ref_value, cancelled).map_err(|error| {
        // libgit2 SSL errors are terse; point the user at the one knob
        // that actually fixes blocked github.com connections.
        if error.contains("SSL") {
            format!(
                "pull template failed: {error}\nhint: github.com may be blocked on this network; configure a mirror with `dshbox config set mirror <prefix>` (e.g. https://gh-proxy.com)"
            )
        } else {
            format!("pull template failed: {error}")
        }
    })?;
    let mut updated = read_config()?;
    updated.selected_dsh_version = Some(version.clone());
    write_config(&updated)?;
    eprintln!(
        "pulled template {} (resolved version {})",
        ref_value, version
    );
    Ok(())
}

pub(crate) fn uninstall_dsh_version(version: &str) -> Result<(), String> {
    if !is_safe_version_name(version) {
        return Err("invalid DSH version".to_owned());
    }
    let mut config = read_config()?;
    let root = config
        .runtime_directory
        .as_deref()
        .ok_or("DSH Box storage is not configured")?;
    let directory = version_directory(root, version)
        .parent()
        .ok_or("invalid DSH destination")?
        .to_path_buf();
    if !directory.is_dir() {
        return Err(format!("DSH version is not installed: {version}"));
    }
    fs::remove_dir_all(&directory)
        .map_err(|error| format!("cannot remove {}: {error}", directory.display()))?;
    if config.selected_dsh_version.as_deref() == Some(version) {
        config.selected_dsh_version = None;
    }
    write_config(&config)?;
    // Data payloads follow template lifecycles: drop store orphans now that
    // this template (and usually its containers) is gone.
    let _ = crate::data::prune_orphaned_data();
    Ok(())
}

/// Catalog helper re-exported for the `dsh search`/`dsh ls` RPCs.
pub(crate) fn catalog_names() -> Result<Vec<String>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut names = read_dsh_catalog(&root);
    for installed in installed_versions(&root)? {
        if !names.contains(&installed) {
            names.push(installed);
        }
    }
    Ok(names)
}

/// Explicitly run the legacy-data migration and report what changed per
/// installed harness (metadata, `.dboxfile`, base template).
pub(crate) fn upgrade_legacy_resources() -> Result<Vec<HarnessUpgradeReport>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    upgrade_legacy_harness(&root)
}
