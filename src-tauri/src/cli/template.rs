//! `dshbox template` — manage the ONE template store: source script
//! templates (pulled/imported) and built templates (the metadata-only
//! product of `dshbox build`).
//!
//! The `install` / `uninstall` / `list` subcommands call
//! `box_template_core` directly so the CLI works without a running
//! daemon (provided a `runtimeDirectory` is configured). Every other
//! action (`show`, `info`, `import`, `export`, `rm`, `prune`) is a thin
//! RPC against the daemon because those touch storage the daemon owns.

use box_api::{TemplateResource, TemplateResourceList};
use serde_json::json;

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(
            "expected template install|uninstall|list|show|info|import|export|rm|prune"
                .to_owned(),
        );
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox template install <ref>     pull + register a template (root or common)");
        println!(
            "dshbox template uninstall <name>  soft-delete + schedule background hard-delete"
        );
        println!("dshbox template list              list all registered templates");
        println!(
            "dshbox template show <name>       script body, or the resource list when built"
        );
        println!(
            "dshbox template info <name>       build timestamp, id, version, labels"
        );
        println!("dshbox template import <archive.tar.gz> [--name <name>]");
        println!("dshbox template export <name> [<dest.tar.gz>]");
        println!("dshbox template rm <name>");
        println!(
            "dshbox template prune             GC data-store snapshots no built template uses"
        );
        return Ok(());
    }
    match action {
        "install" => install(arguments.get(1).ok_or("expected a template reference (e.g. `github.com/deepseek-ai/deepseek-harness:tag`)")?),
        "uninstall" => uninstall(arguments.get(1).ok_or("expected a template name")?),
        "ls" | "list" => list(),
        "show" | "cat" => show(arguments.get(1).ok_or("expected a template name")?),
        "info" => info(arguments.get(1).ok_or("expected a template name")?),
        "import" => import(
            arguments.get(1).ok_or("expected an archive path")?,
            &arguments[2..],
        ),
        "export" => export(
            arguments.get(1).ok_or("expected a template name")?,
            arguments.get(2).map(String::as_str),
        ),
        "rm" | "remove" => remove(arguments.get(1).ok_or("expected a template name")?),
        "prune" => prune(),
        other => Err(format!("unknown template action: {other}")),
    }
}

/// Read the runtime directory directly from the user config. The CLI runs
/// without a daemon for `install` / `uninstall` / `list`, so it cannot
/// rely on the daemon for storage paths.
fn local_runtime() -> Result<String, String> {
    let config = box_foundation::read_config()
        .map_err(|error| format!("cannot read dsh-box config: {error}"))?;
    config
        .runtime_directory
        .ok_or_else(|| "DSH Box storage is not configured; run `dshbox config set runtime <dir>` first".to_owned())
}

/// `dshbox template install <ref>` — pull a template by reference and
/// register it in the local runtime. Goes through `box_template_core` so
/// the same code path the daemon uses is exercised here; the difference
/// is the daemon would also enqueue the work via `box-scheduler` and
/// broadcast progress on `/events`, while the CLI prints a single
/// terminal line on completion.
fn install(ref_value: &str) -> Result<(), String> {
    let runtime = local_runtime()?;
    let ref_value = ref_value.trim();
    if ref_value.is_empty() {
        return Err("template reference cannot be empty".to_owned());
    }
    println!("installing template {ref_value} (this may take a while)...");
    let outcome = box_template_core::install_template(&runtime, ref_value, || false)?;
    let kind = match outcome.entry.kind {
        box_dsh_versions::TemplateKind::Root => "root",
        box_dsh_versions::TemplateKind::Common => "common",
    };
    println!(
        "installed {} ({}, version {}, kind={})",
        outcome.entry.name, outcome.entry.id, outcome.version, kind
    );
    Ok(())
}

/// `dshbox template uninstall <name>` — soft-delete by name. The actual
/// removal of the cloned source happens in the data-scheduler's
/// background deletion queue, so this returns within a few hundred
/// milliseconds even for multi-GB clones.
fn uninstall(name: &str) -> Result<(), String> {
    let runtime = local_runtime()?;
    let (id, path) = box_template_core::uninstall_template(&runtime, name)?;
    if path.is_empty() {
        println!("soft-deleted {name} ({id})");
    } else {
        println!("soft-deleted {name} ({id}); background cleanup scheduled at {path}");
    }
    Ok(())
}

/// `dshbox template list` — local view (no daemon needed). Mirrors the
/// columns the UI shows so `dsh-box` and `dshbox` render the same data.
fn list() -> Result<(), String> {
    let runtime = local_runtime()?;
    let index = box_dsh_versions::read_template_index(&runtime);
    if index.is_empty() {
        println!("(no templates installed)");
        return Ok(());
    }
    println!("NAME\tVERSION\tPROFILE\tKIND\tFORM");
    for entry in index.values() {
        let version = entry.harness_ref.as_deref().unwrap_or("-");
        let kind = match entry.kind {
            box_dsh_versions::TemplateKind::Root => "root",
            box_dsh_versions::TemplateKind::Common => "common",
        };
        let form = if entry.built { "built" } else { "script" };
        println!(
            "{}\t{version}\t{}\t{kind}\t{form}",
            entry.name, entry.profile
        );
    }
    Ok(())
}

fn show(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    // Built templates render their resource list; source scripts their body.
    if let Ok(value) = rpc::call(&client, "read_template_list", json!({ "name": name })) {
        let list: TemplateResourceList = serde_json::from_value(value)
            .map_err(|error| format!("invalid built template from daemon: {error}"))?;
        println!("name:     {}", list.name);
        println!("base:     {}", list.base);
        println!("profile:  {}", list.profile);
        println!("harness:  {}", list.harness_ref.as_deref().unwrap_or("-"));
        println!("created:  {}", list.created_at);
        println!("resources: ({} total)", list.resources.len());
        for (index, resource) in list.resources.iter().enumerate() {
            match resource {
                TemplateResource::Reference {
                    kind,
                    name,
                    version,
                    entry_id,
                } => {
                    println!(
                        "  {}. {kind} {name} {} (reference -> repository entry {entry_id})",
                        index + 1,
                        version.as_deref().unwrap_or("-")
                    );
                }
                TemplateResource::Snapshot {
                    kind,
                    name,
                    digest,
                    destination,
                } => {
                    println!(
                        "  {}. {kind} {name} (snapshot data/{digest} -> {destination})",
                        index + 1
                    );
                }
            }
        }
        return Ok(());
    }
    let value = rpc::call(&client, "read_template", json!({ "name": name }))?;
    let text = value["text"]
        .as_str()
        .ok_or_else(|| "invalid read_template response from daemon".to_owned())?;
    print!("{text}");
    Ok(())
}

fn import(archive: &str, rest: &[String]) -> Result<(), String> {
    let name = flag_value(rest, "--name");
    let client = rpc::connect()?;
    let mut params = json!({ "archive": rpc::absolutize_path(archive) });
    if let Some(name) = name {
        params["name"] = json!(name);
    }
    let value = rpc::call(&client, "import_template", params)?;
    let imported = value["name"].as_str().unwrap_or("(unknown)");
    println!("imported template `{imported}` from {archive}");
    Ok(())
}

fn export(name: &str, destination: Option<&str>) -> Result<(), String> {
    let client = rpc::connect()?;
    let mut params = json!({ "name": name });
    if let Some(destination) = destination {
        params["destination"] = json!(rpc::absolutize_path(destination));
    }
    let value = rpc::call(&client, "export_template", params)?;
    let path = value["path"]
        .as_str()
        .ok_or_else(|| "invalid export_template response from daemon".to_owned())?;
    println!("exported template `{name}` to {path}");
    Ok(())
}

fn remove(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::call(&client, "remove_template", json!({ "name": name }))?;
    println!("removed template `{name}`");
    Ok(())
}

/// GC data-store digests no built template (and no live container)
/// references.
fn prune() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "prune_template_snapshots", json!({}))?;
    let removed: Vec<String> = serde_json::from_value(value["removed"].clone())
        .map_err(|error| format!("invalid prune response from daemon: {error}"))?;
    if removed.is_empty() {
        println!("no unreferenced template snapshots to prune");
    } else {
        for digest in &removed {
            println!("pruned snapshot {digest}");
        }
    }
    Ok(())
}

fn info(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "template_info", json!({ "name": name }))?;

    println!("name:        {}", value["name"].as_str().unwrap_or("-"));
    println!("id:          {}", value["id"].as_str().unwrap_or("-"));
    println!("built:       {}", value["built"].as_bool().unwrap_or(false));

    if value["built"].as_bool() == Some(true) {
        println!("base:        {}", value["base"].as_str().unwrap_or("-"));
        println!("profile:     {}", value["profile"].as_str().unwrap_or("-"));
        println!(
            "harness:     {}",
            value["harnessRef"].as_str().unwrap_or("-")
        );
        println!(
            "schemaVer:   {}",
            value["schemaVersion"].as_u64().unwrap_or(0)
        );
        println!(
            "createdAt:   {} ({})",
            value["createdAt"].as_u64().unwrap_or(0),
            value["createdAtIso"].as_str().unwrap_or("-")
        );
        println!(
            "resources:   {}",
            value["resources"].as_u64().unwrap_or(0)
        );
        if let Some(labels) = value["labels"].as_object() {
            if !labels.is_empty() {
                println!("labels:");
                for (k, v) in labels {
                    println!("  {}: {}", k, v.as_str().unwrap_or("-"));
                }
            }
        }
    } else {
        println!("profile:     {}", value["profile"].as_str().unwrap_or("-"));
        println!(
            "harness:     {}",
            value["harnessRef"].as_str().unwrap_or("-")
        );
        println!(
            "importedAt:  {}",
            value["importedAt"].as_u64().unwrap_or(0)
        );
    }
    Ok(())
}

fn flag_value(arguments: &[String], flag: &str) -> Option<String> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .filter(|value| !value.is_empty())
}
