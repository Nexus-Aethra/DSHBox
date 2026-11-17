//! pnpm build approval: the supply-chain gate AGENTS.md says not to relax.
//!
//! Parses pnpm refusal markers out of a build log, derives the
//! package@version keys that authorise a lifecycle script, and merges them
//! idempotently into a single allowBuilds: section. Every key is quoted,
//! because a scoped spec starts with @ -- a YAML reserved indicator -- and an
//! unquoted one fails the whole manifest.

use super::*;

/// Convert the Boxfile parser's structured source back into a pnpm argument.
/// Resolution is intentionally delegated to pnpm; this function only restores
/// syntax the Boxfile grammar split into fields.
pub(super) fn pnpm_plugin_spec(source: &box_image::ParsedSource) -> Result<String, String> {
    match source {
        box_image::ParsedSource::Github { url, ref_ } => Ok(format!(
            "git+{url}{}",
            ref_.as_deref().map(|value| format!("#{value}")).unwrap_or_default()
        )),
        box_image::ParsedSource::Tarball { url, .. }
        | box_image::ParsedSource::Passthrough { spec: url } => Ok(url.clone()),
        box_image::ParsedSource::BareName { name, scope, version } => {
            let package = scope
                .as_deref()
                .map(|scope| format!("@{scope}/{name}"))
                .unwrap_or_else(|| name.clone());
            Ok(version
                .as_deref()
                .map(|version| format!("{package}@{version}"))
                .unwrap_or(package))
        }
        box_image::ParsedSource::NpmPrefix { spec } => Ok(spec.clone()),
        box_image::ParsedSource::GitPrefix { ref_ } => Ok(format!("git+https://{ref_}")),
        // A `file:` directory is not portable if it remains outside the
        // template. Keeping it as a separate explicit feature is safer than
        // silently publishing a recipe that later points at the builder's
        // machine.
        box_image::ParsedSource::LocalDir { .. } => Err(
            "local plugin directories are not yet portable sealed recipes; pack them first or use a remote pnpm source"
                .to_owned(),
        ),
    }
}

/// Ask the official DSH command to create the profile package/lock recipe,
/// then remove only its materialized package-manager graph. The shared Box
/// pnpm store retains downloaded content; the sealed template retains the
/// portable metadata needed to re-materialize it offline.
/// Extract pnpm's resolved, immutable lifecycle approval key. pnpm owns this
/// identity (including the Git commit tarball URL), so Box never guesses it.
fn pnpm_allow_build_key(log_path: &Path) -> Option<String> {
    let log = fs::read_to_string(log_path).ok()?;
    let (_, after_header) = log.rsplit_once("allowBuilds:")?;
    after_header.lines().find_map(|line| {
        let line = line.trim();
        line.strip_suffix(": true")
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|key| key.trim_matches('"').to_owned())
    })
}

/// pnpm 11.7 reports blocked git-hosted direct dependencies under
/// `[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED]` and follows the error with a
/// copy-pasteable snippet showing the exact key the user is expected to
/// paste under `allowBuilds:`. Extract that key verbatim so the retry
/// writes the form pnpm will accept without further negotiation.
pub(super) fn pnpm_git_allow_build_key(log_path: &Path) -> Option<String> {
    let log = fs::read_to_string(log_path).ok()?;
    let (_, after_header) = log.rsplit_once("[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED]")?;
    let mut lines = after_header.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed == "allowBuilds:" {
            // The first `  <key>: true` line under the snippet header
            // is the exact key pnpm is asking for.
            let key_line = lines.find(|candidate| {
                let candidate = candidate.trim();
                candidate.ends_with(": true") && !candidate.starts_with('#')
            })?;
            return key_line
                .trim()
                .strip_suffix(": true")
                .map(|key| key.trim().trim_matches('"').to_owned());
        }
    }
    None
}

pub(super) fn pnpm_ignored_build_keys(log_path: &Path) -> Vec<String> {
    let Ok(log) = fs::read_to_string(log_path) else {
        return Vec::new();
    };
    let Some((_, after_header)) =
        log.rsplit_once("[ERR_PNPM_IGNORED_BUILDS] Ignored build scripts:")
    else {
        return Vec::new();
    };
    after_header
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .flat_map(|line| line.trim().split(','))
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(super) fn build_approval_hint(source: &str, required_key: Option<&str>) -> String {
    // A plugin's own build script is the common case, and there the ADD spec
    // and the key pnpm asks for are the same string. Repeating it made the
    // suggestion look different from the line the user had already written,
    // so they pasted it again and the build failed again.
    let mut entries: Vec<String> = Vec::new();
    for entry in [Some(source), required_key].into_iter().flatten() {
        if !entries.iter().any(|stored| stored == entry) {
            entries.push(entry.to_owned());
        }
    }
    format!(
        "plugin build scripts require explicit pnpm approval. Add this line to the Boxfile, then build again:\nLABEL dshbox.allow-build={}",
        entries.join(",")
    )
}

/// Idempotently insert `key` into the `pnpm-workspace.yaml` `allowBuilds:`
/// map. Repeated calls merge into a single section: existing approvals are
/// re-emitted in deterministic (sorted) order, the section header is never
/// duplicated, and anything else in the file — the sections around it and any
/// line inside this one that is not an approval — keeps its own text.
pub(super) fn add_pnpm_build_approval(workspace_manifest: &Path, key: &str) -> Result<(), String> {
    let existing = fs::read_to_string(workspace_manifest)
        .map_err(|error| format!("cannot read pnpm workspace manifest: {error}"))?;
    // `allowBuilds:` is a top-level block mapping, and its body is exactly the
    // run of indented lines under the header. Reading it by indentation is what
    // keeps a section that follows it (`minimumReleaseAgeExclude:`, which pnpm
    // appends right after) out of the body, so the next rewrite cannot drop it;
    // and lines Box does not own (a comment, an entry someone else wrote) go
    // back out verbatim instead of vanishing.
    let lines: Vec<&str> = existing.split('\n').collect();
    let header_index = lines
        .iter()
        .position(|line| line.trim_end() == "allowBuilds:" && !is_indented(line));
    let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut foreign: Vec<&str> = Vec::new();
    let body_end = match header_index {
        Some(index) => {
            let mut end = index + 1;
            while end < lines.len() && is_indented(lines[end]) {
                match allow_build_entry_key(lines[end]) {
                    Some(key) => {
                        keys.insert(key);
                    }
                    None => foreign.push(lines[end]),
                }
                end += 1;
            }
            end
        }
        None => 0,
    };
    keys.insert(key.to_owned());
    // Keys must be quoted. A scoped package spec starts with `@`, which YAML
    // reads as a reserved indicator, so the bare form pnpm prints inside its
    // own error block is not valid YAML for us to write back:
    //
    //     allowBuilds:
    //       @deepseek-ai/dsh-subprocess-local@0.2.1-rc.2: true
    //
    // is `bad indentation of a mapping entry`, and the retry that was meant
    // to approve the key dies on the manifest it just corrupted instead of
    // getting further. Verified against the bundled pnpm 11.28.5: quoted keys
    // are accepted and take effect. The read-back above already strips these
    // quotes, so this round-trips.
    let rendered: Vec<String> = keys
        .iter()
        .map(|existing_key| format!("  \"{existing_key}\": true"))
        .collect();
    let mut rebuilt: Vec<String> = Vec::new();
    match header_index {
        Some(index) => {
            rebuilt.extend(lines[..index].iter().map(|line| (*line).to_owned()));
            rebuilt.push("allowBuilds:".to_owned());
            rebuilt.extend(foreign.iter().map(|line| (*line).to_owned()));
            rebuilt.extend(rendered);
            rebuilt.extend(lines[body_end..].iter().map(|line| (*line).to_owned()));
        }
        None => {
            // No section yet: the whole file is the text before it, and the
            // section is appended after its last real line.
            let mut body = lines.clone();
            while body.last().is_some_and(|line| line.trim_end().is_empty()) {
                body.pop();
            }
            rebuilt.extend(body.iter().map(|line| (*line).to_owned()));
            rebuilt.push("allowBuilds:".to_owned());
            rebuilt.extend(rendered);
            rebuilt.push(String::new());
        }
    }
    let updated = rebuilt.join("\n");
    fs::write(workspace_manifest, updated)
        .map_err(|error| format!("cannot write pnpm build approval: {error}"))
}


/// Whether `line` belongs to the block under a mapping header.
fn is_indented(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

/// The package spec approved by one `allowBuilds:` map line, if the line is
/// such an entry. pnpm quotes the specs it writes itself and Box quotes the
/// ones it writes, so only a matching pair of surrounding quotes is stripped —
/// a spec that merely contains a quote travels back out unchanged rather than
/// being rewritten into a different key.
fn allow_build_entry_key(line: &str) -> Option<String> {
    let key = line.trim().strip_suffix(": true")?.trim();
    let unquoted = key
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            key.strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })?;
    (!unquoted.is_empty()).then(|| unquoted.replace("''", "'"))
}


pub(super) fn seed_profile_recipe(
    staged: &Path,
    base_harness: &Path,
    profile: &str,
    plugin_sources: &[String],
    allowed_build_sources: &[String],
    task: &TaskContext,
) -> Result<(), String> {
    if plugin_sources.is_empty() {
        return Ok(());
    }
    let pnpm = resolve_toolchain("pnpm")?;
    let log_path = staged.join("prepare.log");
    ensure_prepared_base_tools(base_harness, &pnpm, &log_path, task)?;
    let profile_home = staged.join("profile").to_string_lossy().into_owned();
    let harness_arg = base_harness.to_string_lossy().into_owned();
    let workspace_manifest = staged
        .join("profile/profiles")
        .join(profile)
        .join("pnpm-workspace.yaml");
    // Entries not equal to an ADD spec are exact pnpm package keys, such as
    // `node-pty@1.1.0`. They cover a user-approved transitive build script.
    for key in allowed_build_sources
        .iter()
        .filter(|key| !plugin_sources.contains(key))
    {
        add_pnpm_build_approval(&workspace_manifest, key)?;
    }
    for (index, source) in plugin_sources.iter().enumerate() {
        task.update(
            format!("Resolving plugin {}/{}", index + 1, plugin_sources.len()),
            48 + ((index * 16) / plugin_sources.len().max(1)) as u8,
        );
        task.log(&format!(
            "resolving plugin {}/{}: {source}",
            index + 1,
            plugin_sources.len()
        ));
        let spec = ProcessSpec::new(pnpm.path.clone())
            .args(&pnpm.arguments)
            .args([
                "--dir",
                harness_arg.as_str(),
                "dsh",
                "plugin",
                "--profile",
                profile,
                "add",
                source.as_str(),
            ])
            .cwd(base_harness)
            .policy(pnpm_policy(&pnpm)?.task_override("DSH_HOME", profile_home.clone()))
            .kind(ExecutionKind::Logged)
            .log_path(&log_path);
        let mut process = run_logged(&spec, "resolve plugin recipe")?;
        let status = process
            .wait_or_kill(
                &TaskCancel(Some(task)),
                Duration::from_secs(600),
                "resolving plugin recipe",
            )
            .map_err(|error| format!("plugin add: {error}"))?;
        if !status.success() {
            if allowed_build_sources.contains(source) {
                // Two pnpm error shapes can land here, and we have to be
                // ready for both:
                //   * `[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED]` — pnpm 11.7
                //     fires this when the *direct* git-hosted dependency
                //     itself needs to run a `prepare` script. The error
                //     block contains a copy-pasteable `allowBuilds:` map
                //     showing the exact key the user is expected to add.
                //   * `[ERR_PNPM_IGNORED_BUILDS] Ignored build scripts: …`
                //     — pnpm 11 fires this when an *indirect* dependency
                //     (e.g. node-pty) needs a native build script.
                //
                // Both can show up in sequence across retries: the
                // direct-dep approval unblocks the plugin's own prepare
                // script and pnpm then proceeds far enough to discover
                // the indirect deps that were hidden behind it. The
                // retry loop keeps auto-deriving keys until pnpm stops
                // asking for new ones, capped at a small number of
                // rounds to keep a malformed manifest from looping
                // forever.
                const MAX_AUTO_APPROVAL_ROUNDS: usize = 6;
                let mut previous_keys: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                let mut last_failed_hint_key: Option<String> = None;
                let mut auto_approval_resolved = false;
                for _round in 0..MAX_AUTO_APPROVAL_ROUNDS {
                    let mut derived_keys: Vec<String> =
                        pnpm_git_allow_build_key(&log_path).into_iter().collect();
                    derived_keys.extend(pnpm_ignored_build_keys(&log_path));
                    derived_keys.sort();
                    derived_keys.dedup();
                    let new_keys: Vec<String> = derived_keys
                        .iter()
                        .filter(|key| !previous_keys.contains(*key))
                        .cloned()
                        .collect();
                    if new_keys.is_empty() {
                        break;
                    }
                    for key in &new_keys {
                        add_pnpm_build_approval(&workspace_manifest, key)?;
                        previous_keys.insert(key.clone());
                    }
                    last_failed_hint_key = derived_keys.first().cloned();
                    let mut retry = run_logged(&spec, "resolve approved plugin recipe")?;
                    let retry_status = retry
                        .wait_or_kill(
                            &TaskCancel(Some(task)),
                            Duration::from_secs(600),
                            "resolving approved plugin recipe",
                        )
                        .map_err(|error| format!("approved plugin add: {error}"))?;
                    if retry_status.success() {
                        auto_approval_resolved = true;
                        break;
                    }
                }
                if auto_approval_resolved {
                    continue;
                }
                if let Some(key) = last_failed_hint_key {
                    return Err(build_approval_hint(source, Some(&key)));
                }
            } else if let Some(key) = pnpm_allow_build_key(&log_path) {
                return Err(build_approval_hint(source, Some(&key)));
            } else if let Some(key) = pnpm_ignored_build_keys(&log_path).first() {
                return Err(build_approval_hint(source, Some(key)));
            }
            return Err(format!("plugin add failed; inspect {}", log_path.display()));
        }
    }
    for path in [staged
        .join("profile/profiles")
        .join(profile)
        .join("node_modules")]
    {
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|error| {
                format!(
                    "cannot remove staged package graph {}: {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}
