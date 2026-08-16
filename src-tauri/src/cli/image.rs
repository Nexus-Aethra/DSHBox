//! `dshbox image` — manage the local image registry.
//!
//! `dshbox build` produces images (metadata-only lists per
//! docs/specs/image-build.md); this module lists, inspects, removes, and
//! prunes them. `image build` stays as an alias of `dshbox build`.

use std::path::Path;

use box_image::registry::{ImageEntry, ImageList};
use box_scheduler::TaskRecord;

use crate::desktop::app::image::{preview_image_script, validate_archive};
use super::rpc;

/// Build an image from a build script. Runs on the daemon; this process
/// only enqueues and polls. `--name` names the image, `--output`
/// additionally exports a portable `.dshimage` archive.
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
    println!("image built from {script_path}");
    Ok(())
}

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected an image action: ls | show | rm | prune | build".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox image ls                 list the local image registry");
        println!("dshbox image show <name>        print one image's resource list");
        println!("dshbox image rm <name>          remove an image (refuses if a container uses it)");
        println!("dshbox image prune              GC data-store snapshots no image references");
        println!("dshbox image build <script.dsh> [--output <path>] [--name <image>]  (alias of dshbox build)");
        return Ok(());
    }
    match action {
        "ls" => list(),
        "show" => {
            let name = arguments.get(1).ok_or("expected an image name")?;
            show(name)
        }
        "rm" => {
            let name = arguments.get(1).ok_or("expected an image name")?;
            remove(name)
        }
        "prune" => prune(),
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
        _ => Err(format!(
            "unknown image action: {action}; try ls | show | rm | prune | build"
        )),
    }
}

fn list() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_images", serde_json::json!({}))?;
    let entries: Vec<ImageEntry> = serde_json::from_value(value)
        .map_err(|error| format!("invalid image list from daemon: {error}"))?;
    println!("ID\tNAME\tBASE\tCREATED");
    for entry in entries {
        println!("{}\t{}\t{}\t{}", entry.id, entry.name, entry.base, entry.created_at);
    }
    Ok(())
}

fn show(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "read_image", serde_json::json!({ "name": name }))?;
    let list: ImageList = serde_json::from_value(value)
        .map_err(|error| format!("invalid image from daemon: {error}"))?;
    println!("name:     {}", list.name);
    println!("base:     {}", list.base);
    println!("profile:  {}", list.profile);
    println!("harness:  {}", list.harness_ref.as_deref().unwrap_or("-"));
    println!("created:  {}", list.created_at);
    println!("resources: ({} total)", list.resources.len());
    for (index, resource) in list.resources.iter().enumerate() {
        match resource {
            box_image::ImageResource::Reference { kind, name, version, entry_id } => {
                println!(
                    "  {}. {kind} {name} {} (reference -> repository entry {entry_id})",
                    index + 1,
                    version.as_deref().unwrap_or("-")
                );
            }
            box_image::ImageResource::Snapshot { kind, name, digest, destination } => {
                println!(
                    "  {}. {kind} {name} (snapshot data/{digest} -> {destination})",
                    index + 1
                );
            }
        }
    }
    Ok(())
}

fn remove(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::call(&client, "remove_image", serde_json::json!({ "name": name }))?;
    println!("removed image `{name}`");
    Ok(())
}

/// GC data-store digests no stored image (and no live container) references.
fn prune() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "prune_image_snapshots", serde_json::json!({}))?;
    let removed: Vec<String> = serde_json::from_value(value["removed"].clone())
        .map_err(|error| format!("invalid prune response from daemon: {error}"))?;
    if removed.is_empty() {
        println!("no unreferenced image snapshots to prune");
    } else {
        for digest in &removed {
            println!("pruned snapshot {digest}");
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
