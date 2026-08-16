//! `dshbox image` — deprecated alias for `dshbox build`.
//!
//! The unified top-level command is `dshbox build <boxfile>`. This module
//! is retained as a backwards-compatible shim while existing scripts and
//! docs catch up. Only the `build` action is honoured; the rest of the
//! pre-deprecation surface (preview/inspect/ls/rm) was a stub and is now
//! removed.

use std::path::Path;

use box_scheduler::TaskRecord;

use crate::desktop::app::image::{preview_image_script, validate_archive};
use super::rpc;

/// Build a container from a build script. The core of the legacy
/// `dshbox image build` action — kept `pub(crate)` so the new
/// `dshbox build` command can delegate to it. The build runs on the
/// daemon; this process only enqueues and polls.
pub(crate) fn build(script_path: &str, output_path: Option<String>, container_name: Option<String>) -> Result<(), String> {
    // The daemon runs with a different CWD; resolve every path here.
    let script = rpc::absolutize_path(script_path);
    let output = output_path.map(|path| rpc::absolutize_path(&path));
    let client = rpc::connect()?;
    let value = rpc::call(
        &client,
        "enqueue_build",
        serde_json::json!({
            "scriptPath": script,
            "outputPath": output,
            "containerName": container_name,
        }),
    )?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    rpc::wait_task(&client, &task.id)?;
    println!("container built from {script_path}");
    Ok(())
}

/// Deprecated entry point. Prints a warning then forwards to `build` for
/// backwards compatibility. `prune` (the data-store garbage collector) is
/// the one non-build action kept here; the rest of the pre-deprecation
/// surface (preview/inspect/ls/rm) was a stub and is now removed.
pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    eprintln!("warning: 'dshbox image' is deprecated, use 'dshbox build' instead.");
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected image build <script>|prune".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox image build <script.dsh> [--output <path.dshimage>] [--name <container-name>]");
        println!("dshbox image prune");
        println!();
        println!("Deprecated alias for 'dshbox build'. `prune` removes data-store");
        println!("blobs no container references.");
        return Ok(());
    }
    match action {
        "build" => {
            let script_path = arguments.get(1).ok_or("expected a script path")?;
            let output_path = arguments
                .windows(2)
                .find(|pair| pair[0] == "--output")
                .map(|pair| pair[1].clone());
            let container_name = arguments
                .windows(2)
                .find(|pair| pair[0] == "--name")
                .map(|pair| pair[1].clone());
            build(script_path, output_path, container_name)
        }
        "prune" => data_prune(),
        _ => Err(format!(
            "unknown image action: {action}; only 'build' and 'prune' remain (use 'dshbox build')"
        )),
    }
}

/// `dshbox image prune` — garbage-collect orphaned data-store blobs via the
/// daemon's `prune_orphaned_data` RPC.
fn data_prune() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "prune_orphaned_data", serde_json::json!({}))?;
    let removed: Vec<String> = serde_json::from_value(value)
        .map_err(|error| format!("invalid prune response from daemon: {error}"))?;
    if removed.is_empty() {
        println!("no orphaned data blobs to prune");
    } else {
        for digest in &removed {
            println!("pruned orphaned data blob {digest}");
        }
    }
    Ok(())
}

// The functions below are preserved (annotated `dead_code`) so future
// preview/inspect/ls/rm actions can be reintroduced without re-deriving
// the formatting from scratch.
#[allow(dead_code)]
fn preview(script_path: &str) -> Result<(), String> {
    let result = preview_image_script(Path::new(script_path))?;
    println!("name:     {}", result.name);
    println!("version:  {}", result.version);
    println!("harness:  {}", result.harness_url);
    println!("profile:  {}", result.profile);
    if !result.labels.is_empty() {
        println!("labels:");
        for (key, value) in &result.labels {
            println!("  {key}={value}");
        }
    }
    println!("operations:");
    for (i, op) in result.ops.iter().enumerate() {
        let kind = op.kind.as_str();
        println!("  {}. ADD {} {}  (line {})", i + 1, kind, op.source, op.line);
    }
    Ok(())
}

#[allow(dead_code)]
fn inspect(archive_path: &str) -> Result<(), String> {
    let manifest = validate_archive(Path::new(archive_path))?;
    println!("id:             {}", manifest.id);
    println!("name:           {}", manifest.name);
    println!("version:        {}", manifest.version);
    println!("schema version: {}", manifest.schema_version);
    println!("media type:     {}", manifest.media_type);
    println!("created at:     {}", manifest.created_at);
    match &manifest.base {
        box_image::TemplateBase::Runtime { ref_ } => {
            println!("harness ref:    {}", ref_.as_deref().unwrap_or("latest"));
        }
        box_image::TemplateBase::Template { id } => println!("base template:  {id}"),
    }
    if !manifest.labels.is_empty() {
        println!("labels:");
        for (key, value) in &manifest.labels {
            println!("  {key}={value}");
        }
    }
    if !manifest.defs.is_empty() {
        println!("defs:");
        for (key, value) in &manifest.defs {
            println!("  {key} = {value}");
        }
    }
    println!("adds: ({} total)", manifest.adds.len());
    for (i, add) in manifest.adds.iter().enumerate() {
        let kind = match add.kind {
            box_image::AddKind::Plugin => "plugin",
            box_image::AddKind::Skill => "skill",
            box_image::AddKind::Data => "data",
        };
        let src = match &add.source {
            box_image::AddSource::Github { url, ref_ } => format!("github:{url}{}", ref_.as_ref().map(|r| format!("@{r}")).unwrap_or_default()),
            box_image::AddSource::Tarball { url, local } => format!("tarball:{} ({})", url, if *local { "local" } else { "remote" }),
            box_image::AddSource::LocalPath { path } => format!("local:{path}"),
            box_image::AddSource::PluginPath { plugin_name, rel_path } => format!("@{plugin_name}@{rel_path}"),
            box_image::AddSource::ContainerPath { container_id, path } => format!("container:{}@{}", container_id.as_deref().unwrap_or("?"), path),
        };
        println!("  {}. ADD {} {}  -> {}  blob={}  digest={}", i + 1, kind, src, add.destination, add.blob, add.digest);
    }
    Ok(())
}

#[allow(dead_code)]
fn list_images() -> Result<(), String> {
    // TODO: scan a saved-images directory for .dshimage archives.
    println!("ID\tNAME\tVERSION\tCREATED");
    println!("(no saved images yet — use 'dshbox build --output <file.dshimage>' to create one)");
    Ok(())
}

#[allow(dead_code)]
fn remove_image(_id: &str) -> Result<(), String> {
    // TODO: remove a saved image by id.
    Err("image removal is not yet implemented".to_owned())
}
