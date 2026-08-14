//! `dshbox bundle` — assemble and ship extension bundles (整合包).

use box_extensions::read_bundles;
use box_foundation::read_config;
use serde_json::json;
use std::path::Path;

use crate::desktop::app::{
    create_extension_bundle, delete_extension_bundle, export_extension_bundle,
    import_extension_bundle, ImportBundleRequest,
};
use super::{flag_value, run_task};

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected bundle ls|create|rm|save|load".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox bundle ls");
        println!("dshbox bundle create <name> --plugin <id> [--plugin <id> ...]");
        println!("dshbox bundle rm <id>");
        println!("dshbox bundle save <id> <dest.tar.gz> [--mode quick|full]");
        println!("dshbox bundle load <archive> [--conflict keep|overwrite]");
        return Ok(());
    }
    match action {
        "ls" | "list" => list_bundles(),
        "create" => create_bundle(&arguments[1..]),
        "rm" => delete_bundle(arguments.get(1).ok_or("expected a bundle id")?),
        "save" => save_bundle(&arguments[1..]),
        "load" | "import" => load_bundle(&arguments[1..]),
        _ => Err(format!("unknown bundle action: {action}")),
    }
}

fn list_bundles() -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    println!("ID\tNAME\tENTRIES\tCREATED");
    for bundle in read_bundles(Path::new(&root)) {
        println!(
            "{}\t{}\t{}\t{}",
            bundle.id,
            bundle.name,
            bundle.entries.len(),
            bundle.created_at
        );
    }
    Ok(())
}

fn create_bundle(arguments: &[String]) -> Result<(), String> {
    let name = arguments.first().ok_or("expected a bundle name")?;
    let ids = arguments
        .windows(2)
        .filter(|pair| pair[0] == "--plugin")
        .map(|pair| pair[1].clone())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err("expected at least one --plugin <id>".to_owned());
    }
    let bundle = create_extension_bundle(name.clone(), ids)?;
    println!("created bundle {} ({})", bundle.name, bundle.id);
    Ok(())
}

fn delete_bundle(name_or_id: &str) -> Result<(), String> {
    let id = resolve_bundle_id(name_or_id)?;
    delete_extension_bundle(id.clone())?;
    println!("deleted bundle {id}");
    Ok(())
}

fn save_bundle(arguments: &[String]) -> Result<(), String> {
    let name_or_id = arguments.first().ok_or("expected a bundle id or name")?;
    let destination = arguments
        .get(1)
        .ok_or("expected a destination .tar.gz path")?;
    let mode = flag_value(arguments, "--mode", "quick");
    if !matches!(mode.as_str(), "quick" | "full") {
        return Err("--mode must be quick or full".to_owned());
    }
    let id = resolve_bundle_id(name_or_id)?;
    let owned_id = id.clone();
    let owned_destination = destination.to_owned();
    let owned_mode = mode.clone();
    run_task(
        "bundle-export",
        vec!["repository:extensions".to_owned()],
        json!({ "bundleId": &id, "destination": destination, "mode": mode }),
        move |task| {
            export_extension_bundle(owned_id, owned_destination, owned_mode, task)
        },
    )?;
    println!("exported bundle {id} to {destination} ({mode} mode)");
    Ok(())
}

fn load_bundle(arguments: &[String]) -> Result<(), String> {
    let archive = arguments.first().ok_or("expected an archive path")?;
    let conflict = flag_value(arguments, "--conflict", "keep");
    if !matches!(conflict.as_str(), "keep" | "overwrite") {
        return Err("--conflict must be keep or overwrite".to_owned());
    }
    let request = ImportBundleRequest {
        archive: archive.to_owned(),
        conflict,
    };
    run_task(
        "bundle-import",
        vec!["repository:extensions".to_owned()],
        json!({ "archive": archive }),
        move |task| import_extension_bundle(request, task),
    )?;
    println!("imported bundle from {archive}");
    Ok(())
}

/// Resolves a bundle given either its id or its display name, docker-style.
fn resolve_bundle_id(name_or_id: &str) -> Result<String, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    read_bundles(Path::new(&root))
        .into_iter()
        .find(|bundle| bundle.id == name_or_id || bundle.name == name_or_id)
        .map(|bundle| bundle.id)
        .ok_or_else(|| format!("bundle not found: {name_or_id}"))
}
