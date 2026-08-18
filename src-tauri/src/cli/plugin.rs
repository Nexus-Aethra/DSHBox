//! `dshbox plugin` — the extension and skill repository, plus per-container
//! plugin installs and listings. Thin client: every action serializes an
//! RPC against the daemon and prints the response.

use serde_json::json;

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected plugin ls|import|export|rm|prune|install|refs".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox plugin ls [container] [--profile <name>]");
        println!("dshbox plugin import <source>");
        println!("dshbox plugin export <id> <dest.tar.gz>");
        println!("dshbox plugin rm <id>");
        println!("dshbox plugin prune");
        println!("dshbox plugin refs [--verbose]");
        println!("dshbox plugin install <container> <source> [--profile <name>]");
        return Ok(());
    }
    match action {
        "ls" | "list" if arguments.len() >= 2 => container_plugins(&arguments[1..]),
        "ls" | "list" => repository_list(),
        "import" => repository_import(
            arguments
                .get(1)
                .ok_or("expected a source: GitHub URL, local directory, or tarball")?,
        ),
        "export" => repository_export(
            arguments.get(1).ok_or("expected a repository entry id")?,
            arguments.get(2).ok_or("expected a destination path")?,
        ),
        "rm" => repository_remove(arguments.get(1).ok_or("expected a repository entry id")?),
        "prune" => repository_prune(),
        "refs" => repository_refs(arguments.iter().skip(1).any(|arg| arg == "--verbose")),
        "install" | "add" => container_plugin_add(&arguments[1..]),
        _ => Err(format!("unknown plugin action: {action}")),
    }
}

fn repository_list() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_repository_extensions", json!({}))?;
    let entries: Vec<box_extensions::RepositoryExtension> = serde_json::from_value(value)
        .map_err(|error| format!("invalid repository list from daemon: {error}"))?;
    println!("ID\tKIND\tNAME\tVERSION");
    for entry in entries {
        println!(
            "{}\t{}\t{}\t{}",
            entry.id,
            kind_name(&entry.kind),
            entry.name,
            entry.version.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

fn kind_name(kind: &box_extensions::ExtensionKind) -> &'static str {
    match kind {
        box_extensions::ExtensionKind::Plugin => "plugin",
        box_extensions::ExtensionKind::Skill => "skill",
    }
}

fn repository_import(source: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::run_task(
        &client,
        "import_repository_extension",
        json!({ "source": rpc::absolutize_path(source) }),
    )?;
    println!("imported repository entry from {source}");
    Ok(())
}

fn repository_export(id: &str, destination: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::run_task(
        &client,
        "export_repository_extension",
        json!({
            "repositoryId": id,
            "destination": rpc::absolutize_path(destination),
        }),
    )?;
    println!("exported repository entry {id} to {destination}");
    Ok(())
}

fn repository_remove(id: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::call(&client, "remove_repository_extension", json!({ "id": id }))?;
    println!("removed repository entry {id}");
    Ok(())
}

/// Print every repository entry alongside the container / template ids
/// that currently reference it. Useful when `plugin rm` or `plugin prune`
/// reports a "still in use" error and the user wants to know which owner
/// is blocking the delete. Pass `--verbose` to expand the id columns.
fn repository_refs(verbose: bool) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_repository_reference_counts", json!({}))?;
    let rows: Vec<box_extensions::RepositoryReferenceRow> = serde_json::from_value(value)
        .map_err(|error| format!("invalid reference rows from daemon: {error}"))?;
    if rows.is_empty() {
        println!("no repository entries");
        return Ok(());
    }
    if verbose {
        println!("ID\tKIND\tNAME\tVERSION\tCONTAINERS\tTEMPLATES");
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}\t[{}]\t[{}]",
                row.id,
                kind_name(&row.kind),
                row.name,
                row.version.as_deref().unwrap_or("-"),
                row.containers.join(", "),
                row.templates.join(", "),
            );
        }
    } else {
        println!("ID\tKIND\tNAME\tVERSION\tCONTAINERS\tTEMPLATES");
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                row.id,
                kind_name(&row.kind),
                row.name,
                row.version.as_deref().unwrap_or("-"),
                row.containers.len(),
                row.templates.len(),
            );
        }
    }
    Ok(())
}

/// Deletes repository entries whose reference count dropped to zero (no
/// container links them anymore). Entries still in use are left untouched.
fn repository_prune() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "prune_repository_extensions", json!({}))?;
    let removed: Vec<String> = serde_json::from_value(value)
        .map_err(|error| format!("invalid prune response from daemon: {error}"))?;
    if removed.is_empty() {
        println!("no unused repository entries to prune");
    } else {
        for id in &removed {
            println!("pruned unused repository entry {id}");
        }
    }
    Ok(())
}

fn container_plugins(arguments: &[String]) -> Result<(), String> {
    let id = arguments.first().ok_or("expected container id")?;
    let profile = selected_profile(arguments);
    let client = rpc::connect()?;
    let value = rpc::call(
        &client,
        "container_list_plugins",
        json!({ "containerId": id, "profile": profile }),
    )?;
    let plugins: Vec<String> = serde_json::from_value(value)
        .map_err(|error| format!("invalid plugin list from daemon: {error}"))?;
    for plugin in plugins {
        println!("{plugin}");
    }
    Ok(())
}

fn container_plugin_add(arguments: &[String]) -> Result<(), String> {
    let id = arguments.first().ok_or("expected container id")?;
    let spec = arguments.get(1).ok_or("expected package or URL")?;
    let profile = selected_profile(arguments);
    let client = rpc::connect()?;
    rpc::run_task(
        &client,
        "container_plugin_add",
        json!({ "containerId": id, "profile": profile, "spec": spec }),
    )?;
    println!("installed {spec} into {id} (profile {profile})");
    Ok(())
}

fn selected_profile(arguments: &[String]) -> String {
    arguments
        .windows(2)
        .find(|pair| pair[0] == "--profile")
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| "web".to_owned())
}
