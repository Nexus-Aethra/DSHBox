//! Container creation for daemon-run tasks. Mirrors the desktop's
//! `containers.rs` shareable core (`create_dsh_container_sync` and the
//! profile scaffolding it needs) plus the startup helpers the daemon
//! lifecycle uses (workspace, context snapshot, profile preflight).

use crate::toolchains::{command_for_toolchain, resolve_toolchain, wait_for_process};
use box_containers::DshContainer;
use box_dsh_context::{
    render_patch_yml, render_snapshot, DshContextFiles, DEFAULT_ORDER, PATCH_FILENAME,
    SNAPSHOT_FILENAME,
};
use box_dsh_versions::version_directory as dsh_version_directory;
use box_foundation::{is_safe_identifier, read_config};
use box_scheduler::TaskContext;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

/// Built-in skill dropped into every freshly-created container so new users
/// can open the workspace and immediately read how a boxfile is written.
/// The body covers every supported directive and source shape so users do
/// not have to leave the container to consult documentation.
const BOXFILE_GUIDE_SKILL: &str = r#"---
name: boxfile-guide
description: How to author a DSH Box boxfile (`.dsh`) — directives, source shapes, and best practices.
---

# DSH Box Boxfile Guide

A **boxfile** is a `.dsh` script describing one container. It mirrors a
Dockerfile: a base template provides the runtime layout, and `ADD` lines
layer extensions on top. Building produces an **image** (metadata only:
plugins are referenced from the shared repository, every other resource is
snapshotted into the data store); containers are created from images:

```
dshbox init              # generate a starter boxfile in the cwd
dshbox pull template <ref>
dshbox build ./boxfile.dsh --name my-image
dshbox run my-image      # or a template name (builds implicitly)
```

## Directives

### FROM <base>

Pick the base template. Use a GitHub short-form ref (host + owner + repo +
optional tag); a tag defaults to `:latest`. The base template is itself a
`.dsh` script that you pulled earlier with `dshbox pull template`.

```
FROM github.com/deepseek-ai/deepseek-harness:latest
```

### PROFILE <name>

Pick the runtime layout (`web`, `cli`, `headless`, ...). The profile
decides which directories get mounted and which template bundles are
materialised into the container.

```
PROFILE web
```

### ADD <kind> <source> [@<dest>]

Layer one extension (plugin, skill, or data) into the container.

```
ADD plugin github.com/foo/bar@v1.0.0
ADD skill @my-skill
ADD data ./datasets/seed.csv
```

`kind` is one of:

| kind   | meaning                                                        |
| ------ | -------------------------------------------------------------- |
| plugin | npm-style JavaScript plugin (mounted at `@plugin`)             |
| skill  | SKILL.md-style knowledge pack (mounted at `@skill`)            |
| data   | payload copied into the container data dir (never linked)      |

`source` accepts any of:

| shape              | example                                            |
| ------------------ | -------------------------------------------------- |
| bare name          | `ADD plugin @my-plugin` (already in the repository) |
| GitHub short form  | `ADD plugin github.com/foo/bar@v1.0.0`              |
| local absolute     | `ADD plugin /home/me/code/my-plugin`               |
| local relative     | `ADD plugin ./relative/path` or `../relative/path` |
| local tarball      | `ADD plugin file:///home/me/archives/foo.tar.gz`  |
| remote tarball     | `ADD plugin https://example.com/foo.tar.gz`       |

The trailing `@<dest>` is an optional destination path override.

## Best practices

* Always pin a tag (`github.com/foo/bar@v1.0.0`) for reproducibility; a
  bare ref resolves to HEAD and drifts with every pull.
* Keep one boxfile per container; reuse base templates instead of
  duplicating ADD lists across multiple boxfiles.
* Use `dshbox plugin ls` to check what is already in the local repository
  before reaching for a GitHub short form.
* Data payloads (`ADD data ...`) are copied verbatim — never linked — so
  they do not pollute the shared repository.
* A boxfile that only contains `FROM` + `PROFILE` is a valid "base only"
  build; useful for trying out a freshly-pulled template.

## DSH Box CLI quick reference

DSH Box ships the `dshbox` binary on PATH; every command talks to the
local daemon, so the same state is shared with the desktop GUI.

```
# Workflow
dshbox init                              # scaffold a boxfile.dsh here
dshbox pull template <owner/repo>[:tag]  # fetch a base template
dshbox build [boxfile.dsh] [--name img]  # build an IMAGE (no container yet)
dshbox run <image|template>              # create + start a container

# Images
dshbox image ls                          # list the local image registry
dshbox image show <name>                 # print an image's resource list
dshbox image rm <name>                   # remove an image
dshbox image prune                       # GC unreferenced snapshots

# Templates
dshbox template ls                       # list local templates
dshbox template show <name>              # print a template's script
dshbox template export <name> [dest]     # save a template as tarball
dshbox template import <file.tar.gz>     # install a template tarball
dshbox template rm <name>                # remove a template

# Extensions
dshbox plugin ls                         # list repository plugins/skills
dshbox plugin import <source>            # add from dir / tarball / github
dshbox bundle ls                         # list extension bundles

# This container
dshbox ps                                # list containers + status
dshbox container url <id>                # webview URL of a running host
dshbox container logs <id>               # tail the DSH host log
dshbox container stop <id>               # stop a running container
dshbox container start <id>              # start a stopped container
```

Run `dshbox help` for the full command reference, or
`dshbox <command> help` for action-level usage.
"#;

const BOXFILE_GUIDE_SKILL_NAME: &str = "boxfile-guide";

pub(crate) fn is_safe_version_name(version: &str) -> bool {
    is_safe_identifier(version)
}

/// Create a container without any UI side-effects. This is the shareable
/// core that both the daemon RPC and the desktop command use.
pub(crate) fn create_dsh_container_sync(
    name: &str,
    version: &str,
    profile: &str,
) -> Result<DshContainer, String> {
    let name = name.trim().to_owned();
    if !is_safe_version_name(version) {
        return Err("invalid DSH version".to_owned());
    }
    if name.is_empty() || name.len() > 80 {
        return Err("container name must contain 1 to 80 characters".to_owned());
    }
    if !is_safe_identifier(profile) {
        return Err("profile must use letters, numbers, dots, dashes, or underscores".to_owned());
    }
    let config = read_config()?;
    let root = config
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    // The completion marker (written after a successful clone) is the
    // installed criterion — `.git` exists from the moment a clone starts,
    // so keying on it would let containers build against a half-downloaded
    // harness.
    if !dsh_version_directory(&root, version)
        .join(".dshbox-runtime.json")
        .is_file()
    {
        return Err(format!("DSH version is not installed: {version}"));
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let id = format!("container-{timestamp}");
    let directory = std::path::PathBuf::from(&root).join("instances").join(&id);
    for name in ["profile", "workspace", "logs", "state"] {
        fs::create_dir_all(directory.join(name))
            .map_err(|error| format!("cannot create container: {error}"))?;
    }
    create_profile_manifest(&directory, profile)?;
    let metadata = serde_json::json!({
        "id": id,
        "name": name,
        "version": version,
        "profile": profile,
        "source": dsh_version_directory(&root, version),
    });
    fs::write(
        directory.join("container.json"),
        serde_json::to_string_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write container metadata: {error}"))?;
    // Drop the built-in boxfile-guide skill into the freshly-created
    // container so first-time users can open the workspace and immediately
    // read how a boxfile is written. The skill is a per-container copy so
    // edits stay local; the source string is bundled with the daemon.
    write_boxfile_guide_skill(&directory)?;
    Ok(DshContainer {
        id,
        name,
        version: version.to_owned(),
        profile: profile.to_owned(),
        template: None,
        directory: directory.to_string_lossy().into_owned(),
        status: "stopped".to_owned(),
    })
}

/// Write the bundled boxfile-guide skill under
/// `<container>/profile/skills/boxfile-guide/SKILL.md`. Idempotent: a
/// pre-existing copy is left untouched so users who edited the file keep
/// their changes.
fn write_boxfile_guide_skill(container_directory: &Path) -> Result<(), String> {
    let destination = container_directory
        .join("profile/skills")
        .join(BOXFILE_GUIDE_SKILL_NAME);
    let skill_md = destination.join("SKILL.md");
    if skill_md.is_file() {
        return Ok(());
    }
    fs::create_dir_all(&destination)
        .map_err(|error| format!("cannot create skill directory: {error}"))?;
    fs::write(&skill_md, BOXFILE_GUIDE_SKILL)
        .map_err(|error| format!("cannot write boxfile-guide skill: {error}"))
}

pub(crate) fn create_profile_manifest(
    container_directory: &Path,
    profile: &str,
) -> Result<(), String> {
    let directory = container_directory.join("profile/profiles").join(profile);
    if directory.exists() {
        return Err(format!("profile already exists: {profile}"));
    }
    fs::create_dir_all(&directory).map_err(|error| format!("cannot create profile: {error}"))?;
    let manifest = serde_json::json!({
        "name": format!("dsh-profile-{profile}"),
        "private": true,
        "dependencies": {},
        "dsh": { "profile": { "bundles": profile_template_bundles(profile) } }
    });
    fs::write(
        directory.join("package.json"),
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write profile manifest: {error}"))?;
    write_profile_support_files(&directory)
}

pub(crate) fn profile_template_bundles(profile: &str) -> Vec<&'static str> {
    match profile {
        "web" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"],
        "headless" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"],
        _ => vec!["@deepseek-ai/dsh-base"],
    }
}

pub(crate) fn write_profile_support_files(directory: &Path) -> Result<(), String> {
    let patch = directory.join("cordis.patch.yml");
    if !patch.exists() {
        fs::write(&patch, "# User overrides for this DSH profile.\n[]\n")
            .map_err(|error| format!("cannot write profile patch: {error}"))?;
    }
    let workspace = directory.join("pnpm-workspace.yaml");
    if !workspace.exists() {
        fs::write(
            &workspace,
            "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n",
        )
        .map_err(|error| format!("cannot write profile workspace: {error}"))?;
    }
    Ok(())
}

pub(crate) fn ensure_container_workspace(directory: &Path) -> Result<PathBuf, String> {
    let workspace = directory.join("workspace");
    fs::create_dir_all(&workspace)
        .map_err(|error| format!("cannot create container workspace: {error}"))?;
    Ok(workspace)
}

/// Render the per-container JSON snapshot Box writes on every container start.
/// The snapshot becomes a `dsh-box:container` PromptContext section (order
/// 130) that the agent receives as a user-role history snapshot.
pub(crate) fn write_dshbox_context_snapshot(
    directory: &Path,
    container: &serde_json::Value,
    profile: &str,
) -> Result<DshContextFiles, String> {
    let workspace = ensure_container_workspace(directory)?;
    let container_name = container["name"].as_str().unwrap_or("DSH Container");
    let container_id = container["id"].as_str().unwrap_or("unknown");
    let version = container["version"].as_str().unwrap_or("unknown");
    let profile_home = directory.join("profile");
    let plugins_root = directory.join("extensions/plugins");
    let skills_root = directory.join("profile/skills");
    let logs_root = directory.join("logs");

    // Read the env-var names Box already wrote into the container's
    // .credentials.yaml via the DSH settings UI; only the names ship.
    let api_key_envs = read_credentials_env_names(&profile_home);

    let state_dir = directory.join("state");
    fs::create_dir_all(&state_dir)
        .map_err(|error| format!("cannot create {}: {error}", state_dir.display()))?;
    let snapshot_path = state_dir.join(SNAPSHOT_FILENAME);
    let patch_path = state_dir.join(PATCH_FILENAME);

    let snapshot_body = render_snapshot(
        container_id,
        container_name,
        version,
        profile,
        &workspace,
        &profile_home,
        &plugins_root,
        &skills_root,
        &logs_root,
        &api_key_envs,
    );
    // Atomic write: stage to .tmp then rename so a racing read never sees a
    // half-written snapshot.
    let snapshot_tmp = snapshot_path.with_extension("json.tmp");
    fs::write(&snapshot_tmp, snapshot_body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", snapshot_tmp.display()))?;
    fs::rename(&snapshot_tmp, &snapshot_path)
        .map_err(|error| format!("cannot rename {}: {error}", snapshot_tmp.display()))?;

    let patch_body = render_patch_yml(&snapshot_path, DEFAULT_ORDER);
    let patch_tmp = patch_path.with_extension("yml.tmp");
    fs::write(&patch_tmp, patch_body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", patch_tmp.display()))?;
    fs::rename(&patch_tmp, &patch_path)
        .map_err(|error| format!("cannot rename {}: {error}", patch_tmp.display()))?;

    Ok(DshContextFiles { snapshot_path, patch_path })
}

/// Extract the `apiKeyEnv` names that the DSH settings UI wrote into
/// `<DSH_HOME>/.credentials.yaml`. Tolerant of missing or malformed files.
fn read_credentials_env_names(profile_home: &Path) -> Vec<String> {
    let path = profile_home.join(".credentials.yaml");
    let body = match fs::read_to_string(&path) {
        Ok(body) => body,
        Err(_) => return Vec::new(),
    };
    let value: serde_yaml::Value = match serde_yaml::from_str(&body) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut names = Vec::new();
    if let Some(map) = value.as_mapping() {
        for (key, _) in map {
            if let Some(key) = key.as_str() {
                names.push(key.to_owned());
            }
        }
    }
    names.sort();
    names
}

/// Repairs Box-created, empty named profiles from builds before profile
/// templates were persisted.
pub(crate) fn repair_known_profile_template(
    container_directory: &Path,
    profile: &str,
) -> Result<(), String> {
    if !matches!(profile, "web" | "headless") {
        return Ok(());
    }
    let directory = container_directory.join("profile/profiles").join(profile);
    let manifest_path = directory.join("package.json");
    let mut manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .map_err(|error| format!("cannot read profile: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile: {error}"))?;
    let empty = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .is_some_and(Vec::is_empty);
    if empty {
        manifest["dsh"]["profile"]["bundles"] =
            serde_json::json!(profile_template_bundles(profile));
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("cannot repair profile: {error}"))?;
    }
    write_profile_support_files(&directory)
}

/// Ensures every non-bundled DSH plugin selected by a profile has its
/// declared runtime entry, preparing TypeScript sources before the DSH
/// loader attempts to import them.
pub(crate) fn preflight_profile_plugins(
    container_directory: &Path,
    profile: &str,
    task: Option<&TaskContext>,
) -> Result<(), String> {
    let profile_directory = container_directory.join("profile/profiles").join(profile);
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(profile_directory.join("package.json"))
            .map_err(|error| format!("cannot read profile manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse profile manifest: {error}"))?;
    let bundles = manifest
        .pointer("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array)
        .ok_or("profile manifest has no dsh.profile.bundles")?;
    for bundle in bundles.iter().filter_map(serde_json::Value::as_str) {
        if bundle.starts_with("@deepseek-ai/") {
            continue;
        }
        let plugin_directory = profile_directory.join("node_modules").join(bundle);
        let plugin_manifest_path = plugin_directory.join("package.json");
        if !plugin_manifest_path.is_file() {
            return Err(format!(
                "profile plugin {bundle} is not installed; re-add it from Container details"
            ));
        }
        let plugin_manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(&plugin_manifest_path)
                .map_err(|error| format!("cannot read plugin {bundle} manifest: {error}"))?,
        )
        .map_err(|error| format!("cannot parse plugin {bundle} manifest: {error}"))?;
        let Some(entry) = plugin_runtime_entry(&plugin_manifest) else {
            continue;
        };
        if plugin_directory.join(&entry).is_file() {
            continue;
        }
        if let Some(task) = task {
            task.update(format!("Preparing plugin {bundle}"), 32);
            task.log(&format!(
                "plugin {bundle} entry {entry} is missing; installing dependencies and building its source"
            ));
            prepare_plugin_source(&plugin_directory, bundle, &entry, task)?;
        } else {
            return Err(format!(
                "plugin {bundle} has no built entry {entry}; start it from DSH Box so it can be prepared"
            ));
        }
    }
    Ok(())
}

pub(crate) fn plugin_runtime_entry(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("main")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            manifest
                .pointer("/exports/./default")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
}

pub(crate) fn prepare_plugin_source(
    directory: &Path,
    name: &str,
    entry: &str,
    task: &TaskContext,
) -> Result<(), String> {
    let pnpm = resolve_toolchain("pnpm")?;
    let task_record = task.manager.task(&task.task_id)?;
    let log = fs::OpenOptions::new()
        .append(true)
        .open(&task_record.log_path)
        .map_err(|error| error.to_string())?;
    let frozen = if directory.join("pnpm-lock.yaml").is_file() {
        "--frozen-lockfile"
    } else {
        "--no-frozen-lockfile"
    };
    let mut install = command_for_toolchain(&pnpm)
        .args([
            "--dir",
            directory.to_string_lossy().as_ref(),
            "install",
            frozen,
        ])
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .spawn()
        .map_err(|error| format!("cannot install dependencies for plugin {name}: {error}"))?;
    let status = wait_for_process(&mut install, Some(task), "installing plugin dependencies")?;
    if !status.success() {
        return Err(format!(
            "plugin {name} dependency installation exited with {status}"
        ));
    }
    if directory.join(entry).is_file() {
        return Ok(());
    }
    if plugin_has_script(directory, "build")? {
        task.update(format!("Building plugin {name}"), 38);
        let mut build = command_for_toolchain(&pnpm)
            .args([
                "--dir",
                directory.to_string_lossy().as_ref(),
                "run",
                "build",
            ])
            .stdout(Stdio::from(
                log.try_clone().map_err(|error| error.to_string())?,
            ))
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|error| format!("cannot build plugin {name}: {error}"))?;
        let status = wait_for_process(&mut build, Some(task), "building plugin")?;
        if !status.success() {
            return Err(format!("plugin {name} build exited with {status}"));
        }
    }
    if directory.join(entry).is_file() {
        Ok(())
    } else {
        Err(format!(
            "plugin {name} build completed but did not create its declared entry {entry}"
        ))
    }
}

pub(crate) fn plugin_has_script(directory: &Path, script: &str) -> Result<bool, String> {
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(directory.join("package.json"))
            .map_err(|error| format!("cannot read plugin manifest: {error}"))?,
    )
    .map_err(|error| format!("cannot parse plugin manifest: {error}"))?;
    Ok(manifest
        .pointer(&format!("/scripts/{script}"))
        .and_then(serde_json::Value::as_str)
        .is_some())
}
