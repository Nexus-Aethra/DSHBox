//! `dshbox template` — manage the ONE template store: source script
//! templates (pulled/imported) and built templates (the metadata-only
//! product of `dshbox build`).
//!
//! Every subcommand is a thin RPC against the daemon; the CLI is
//! deliberately aligned with the UI so any change in storage semantics
//! lands in exactly one place (`dshboxd`).

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

/// Read the runtime directory directly from the user config. Kept as a
/// diagnostic helper — production paths should always go through the
/// daemon so storage layout stays in one place.
#[allow(dead_code)]
fn local_runtime() -> Result<String, String> {
    let config = box_foundation::read_config()
        .map_err(|error| format!("cannot read dsh-box config: {error}"))?;
    config
        .runtime_directory
        .ok_or_else(|| "DSH Box storage is not configured; run `dshbox config set runtime <dir>` first".to_owned())
}

/// `dshbox template install <ref>` — pull a template by reference and
/// register it in the local runtime. Routes through `pull_template`,
/// which the daemon runs inside a task worker so the same code path the
/// UI exercises is hit here too; `run_task` blocks until completion and
/// streams progress on stderr.
fn install(ref_value: &str) -> Result<(), String> {
    let ref_value = ref_value.trim();
    if ref_value.is_empty() {
        return Err("template reference cannot be empty".to_owned());
    }
    println!("installing template {ref_value} (this may take a while)...");
    let client = rpc::connect()?;
    rpc::run_task(
        &client,
        "pull_template",
        json!({ "ref": ref_value }),
    )?;
    println!("installed {ref_value}");
    Ok(())
}

/// `dshbox template uninstall <name>` — soft-delete by name. The actual
/// removal of the cloned source happens in the data-scheduler's
/// background deletion queue, so this returns within a few hundred
/// milliseconds even for multi-GB clones.
fn uninstall(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    // `remove_template` is the daemon-owned soft-delete; running it via
    // `run_task` ensures progress and final status are surfaced the same
    // way as every other long-running template RPC.
    rpc::run_task(&client, "remove_template", json!({ "name": name }))?;
    println!("uninstalled {name}");
    Ok(())
}

/// `dshbox template list` — mirror the UI columns; data comes from the
/// daemon's `list_templates` so the CLI cannot drift from the store.
/// Rendered from raw JSON so we don't have to mirror a Rust type here.
fn list() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_templates", json!({}))?;
    let entries = value.as_array().ok_or_else(|| {
        "invalid list_templates response from daemon: expected an array".to_owned()
    })?;
    if entries.is_empty() {
        println!("(no templates installed)");
        return Ok(());
    }
    println!("NAME\tHARNESS\tPROFILE\tKIND");
    for entry in entries {
        let name = entry["name"].as_str().unwrap_or("?");
        let version = entry["harnessRef"].as_str().unwrap_or("-");
        let profile = entry["profile"].as_str().unwrap_or("-");
        let kind = if entry["built"].as_bool().unwrap_or(false) {
            "sealed"
        } else {
            "prepared"
        };
        println!("{name}\t{version}\t{profile}\t{kind}");
    }
    Ok(())
}

fn show(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
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

    println!("base:        {}", value["base"].as_str().unwrap_or("-"));
    println!("base id:     {}", value["baseId"].as_str().unwrap_or("-"));
    println!("profile:     {}", value["profile"].as_str().unwrap_or("-"));
    println!("schema:      {}", value["schemaVersion"].as_u64().unwrap_or(0));
    println!("created:     {}", value["createdAt"].as_u64().unwrap_or(0));
    println!("size bytes:  {}", value["sizeBytes"].as_u64().unwrap_or(0));
    if let Some(artifacts) = value["pluginArtifacts"].as_array() {
        println!("plugins:     {}", artifacts.len());
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
