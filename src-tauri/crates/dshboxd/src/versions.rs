//! DSH version management for the daemon: catalog refresh, install, and
//! uninstall. Mirrors the desktop's `versions.rs` + `commands/versions.rs`
//! without any Tauri dependency.
//!
//! Templates are the single source of truth: harness clones are mirrored
//! into the template index by `pull_template`, and the Harness tab in the
//! UI is a derived view over that index (no separate `dsh-catalog.json`
//! or legacy "base template" writer any more). The remote tag list is
//! fetched on demand for the "Load versions" button — it is never
//! persisted.

use box_dsh_versions::{
    installed_versions, parse_template_ref, pull_template, read_template_index,
    version_directory, write_template_index, DshVersion, HARNESS_STANDARD_REF,
};
use box_foundation::{is_safe_identifier, mirror_url, now_seconds, read_config, write_config};
use serde::Deserialize;
use std::{fs, time::Duration};

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    box_foundation::is_safe_identifier(version)
}

#[derive(Deserialize)]
struct GitHubTag {
    name: String,
}

/// Resolve the canonical FROM reference into its mirror-aware clone URL.
/// Centralised here so the CLI and the desktop don't reach into
/// `box_dsh_versions` themselves.
fn mirror_url_for(ref_value: &str) -> Result<String, String> {
    let parsed = parse_template_ref(ref_value)
        .map_err(|error| format!("invalid harness reference: {error}"))?;
    let mirror = read_config().ok().and_then(|config| config.github_mirror);
    Ok(mirror_url(&parsed.url, mirror.as_deref()))
}

/// Fetch the harness version catalog. Primary path is the git protocol via
/// libgit2 (`ls-remote` equivalent, no system git executable, no GitHub API
/// rate limits): the harness repo's tags ARE the version list. The GitHub
/// REST API stays as a fallback for networks where the git transport is
/// blocked but plain HTTPS works.
///
/// The returned list is **never** persisted — it is shown only when the
/// user explicitly asks to "Load versions", so a one-off network outage
/// does not produce a stale catalog on disk.
pub(crate) fn fetch_remote_dsh_tags() -> Result<Vec<String>, String> {
    let config = read_config()?;
    let target = mirror_url_for(HARNESS_STANDARD_REF)?;
    match box_runtime::list_remote_tags(&target) {
        Ok(tags) => {
            let filtered: Vec<String> = tags
                .into_iter()
                .filter(|name| is_safe_version_name(name))
                .collect();
            if !filtered.is_empty() {
                return Ok(filtered);
            }
            // Repo reachable but tagless: fall through to the API so a
            // legitimately empty ls-remote does not blank the catalog.
        }
        Err(error) => {
            eprintln!("dshboxd: git tag listing failed ({error}); falling back to the GitHub API");
        }
    }
    fetch_remote_dsh_tags_api(config.github_mirror.as_deref())
}

fn fetch_remote_dsh_tags_api(github_mirror: Option<&str>) -> Result<Vec<String>, String> {
    use box_dsh_versions::DSH_TAGS_API;
    let endpoint = mirror_url(DSH_TAGS_API, github_mirror);
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

/// In-memory refresh kept around as a thin wrapper so the existing
/// `dsh-catalog-refresh` task semantics do not change for the UI. It used
/// to write `state/dsh-catalog.json`; now it just touches the network
/// and prints the result (no persistence, no IPC return value of
/// consequence — the UI re-queries `list_dsh_catalog` for the merged
/// list right after the task settles).
pub(crate) fn refresh_dsh_catalog() -> Result<(), String> {
    fetch_remote_dsh_tags().map(|_| ())
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

/// Uninstall a DSH harness: drop the runtime clone, the template index
/// entry that points at it (so the Harness tab and the Container dropdown
/// both stop surfacing the version), and prune orphan data payloads.
///
/// The runtime directory and the template index are now treated as a
/// single resource — neither half stays behind when the other is gone.
pub(crate) fn uninstall_dsh_version(version: &str) -> Result<(), String> {
    if !is_safe_version_name(version) {
        return Err("invalid DSH version".to_owned());
    }
    let mut config = read_config()?;
    let root = config
        .runtime_directory
        .as_deref()
        .ok_or("DSH Box storage is not configured")?;
    // 1. Drop the runtime clone.
    let directory = version_directory(root, version)
        .parent()
        .ok_or("invalid DSH destination")?
        .to_path_buf();
    if directory.is_dir() {
        fs::remove_dir_all(&directory)
            .map_err(|error| format!("cannot remove {}: {error}", directory.display()))?;
    }
    // 2. Drop the matching template index entry. Every pulled harness
    // produces a template entry whose `harness_ref` matches the version
    // slug, so this is the single lookup that unifies the two stores.
    let mut index = read_template_index(root);
    let names_to_remove: Vec<String> = index
        .iter()
        .filter_map(|(name, entry)| {
            if entry.harness_ref.as_deref() == Some(version) {
                Some(name.clone())
            } else {
                None
            }
        })
        .collect();
    for name in &names_to_remove {
        index.remove(name);
    }
    if !names_to_remove.is_empty() {
        write_template_index(root, &index)?;
    }
    if config.selected_dsh_version.as_deref() == Some(version) {
        config.selected_dsh_version = None;
    }
    write_config(&config)?;
    // Data payloads follow template lifecycles: drop store orphans now that
    // this template (and usually its containers) is gone.
    let _ = crate::data::prune_orphaned_data();
    Ok(())
}

/// Build the `DshVersion[]` payload the Harness tab shows. Every entry is
/// derived from the template index: name = `harness_ref`, `installed` =
/// `<runtime>/runtimes/<tag>/source/.dshbox-runtime.json` exists.
///
/// Remote tags fetched by `refresh_dsh_catalog` are merged in (always
/// marked `installed = false` unless the template index says otherwise)
/// so users still see new releases between pulls.
pub(crate) fn list_dsh_versions_derived(
    remote_tags: Option<Vec<String>>,
) -> Result<Vec<DshVersion>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;

    // Index-derived entries (authoritative source).
    let index = read_template_index(&root);
    let mut by_name: std::collections::BTreeMap<String, DshVersion> =
        std::collections::BTreeMap::new();
    for entry in index.values() {
        let Some(tag) = entry.harness_ref.as_deref() else {
            continue;
        };
        if !is_safe_version_name(tag) {
            continue;
        }
        let installed = version_directory(&root, tag)
            .join(".dshbox-runtime.json")
            .is_file();
        by_name.entry(tag.to_owned())
            .and_modify(|existing| existing.installed = existing.installed || installed)
            .or_insert(DshVersion {
                name: tag.to_owned(),
                installed,
            });
    }

    // Remote-only entries (always uninstalled).
    if let Some(tags) = remote_tags {
        for tag in tags {
            if !is_safe_version_name(&tag) {
                continue;
            }
            by_name.entry(tag.clone()).or_insert(DshVersion {
                name: tag,
                installed: false,
            });
        }
    }

    // Make sure `latest` shows up even when nothing is installed yet —
    // it's the implicit default branch reference.
    by_name.entry("latest".to_owned()).or_insert(DshVersion {
        name: "latest".to_owned(),
        installed: version_directory(&root, "latest")
            .join(".dshbox-runtime.json")
            .is_file(),
    });

    Ok(by_name.into_values().collect())
}

/// Just the `installed` subset — useful for the Container page badge.
pub(crate) fn list_installed_dsh_versions() -> Result<Vec<String>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(installed_versions(&root)?)
}

/// One-time migration for users that have runtime clones but no template
/// index entries (older installs that used the legacy `install_dsh_version`
/// path). For every `runtimes/<tag>/source/.dshbox-runtime.json` that does
/// not yet have a template index entry, register one with the canonical
/// `FROM github.com/deepseek-ai/deepseek-harness:<tag>` body.
///
/// Idempotent: a second call is a no-op once every runtime is mirrored.
/// Returns the tags that were registered in this pass so the caller can
/// surface a diagnostic message.
pub(crate) fn migrate_runtime_runtimes_to_templates() -> Result<Vec<String>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let index = read_template_index(&root);
    let existing: std::collections::BTreeSet<String> = index
        .values()
        .filter_map(|entry| entry.harness_ref.clone())
        .collect();
    let mut registered = Vec::new();
    for tag in installed_versions(&root)? {
        if existing.contains(&tag) {
            continue;
        }
        let ref_value = format!("github.com/deepseek-ai/deepseek-harness:{tag}");
        let body = format!("FROM {ref_value}\nPROFILE web\nNAME {ref_value}\nVERSION latest\n");
        // Drop the body into the hash-addressed store, mirroring what
        // `pull_template` does. `write_template_with_entry` also updates
        // the manifest and index, so no extra bookkeeping is needed.
        box_dsh_versions::write_template_with_entry(
            &root,
            &ref_value,
            &body,
            Some(tag.clone()),
            "web",
            Some(ref_value.clone()),
            now_seconds(),
        )
        .map_err(|error| format!("cannot register migrated template for `{tag}`: {error}"))?;
        registered.push(tag);
    }
    Ok(registered)
}