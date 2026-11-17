//! The remote DSH version catalogue: what tags exist upstream, and what is
//! installed locally.
//!
//! Read-only with respect to the machine. Fetching tags is all it does; preparing
//! a tree from one of them is mod.rs's job.

use super::*;
use super::prepared_base_index::{read_prepared_base_index, GitHubTag};

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

    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    let index = read_prepared_base_index(&layout)?;
    let mut by_name: std::collections::BTreeMap<String, DshVersion> =
        std::collections::BTreeMap::new();
    for record in index.values() {
        if !is_safe_version_name(&record.version) {
            continue;
        }
        let installed = PathBuf::from(&record.directory)
            .join("manifest.json")
            .is_file();
        by_name
            .entry(record.version.clone())
            .and_modify(|existing| existing.installed = existing.installed || installed)
            .or_insert(DshVersion {
                name: record.version.clone(),
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
        installed: false,
    });

    // Collected by name for dedup, which is also byte order — oldest first, and
    // `latest` last because `l` sorts after `d`. Ordered for reading instead.
    let mut versions: Vec<DshVersion> = by_name.into_values().collect();
    sort_catalog(&mut versions);
    Ok(versions)
}

/// Just the `installed` subset — useful for the Container page badge.
pub(crate) fn list_installed_dsh_versions() -> Result<Vec<String>, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let layout = RuntimeLayout::new(&root);
    layout.initialize_schema_10()?;
    Ok(read_prepared_base_index(&layout)?.into_keys().collect())
}
