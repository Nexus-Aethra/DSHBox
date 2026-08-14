//! `dshbox plugin` — the extension and skill repository, plus per-container
//! plugin installs and listings.

use box_extensions::{scan_repository, write_repository_index, ExtensionKind};
use box_foundation::{read_config, suppress_console_window};
use box_dsh_versions::version_directory;
use serde_json::{json, Value};
use std::{env, fs, path::Path, path::PathBuf, process::Command};

use crate::desktop::app::{
    export_repository_extension, import_repository_extension, ExportRepositoryExtensionRequest,
    ImportRepositoryExtensionRequest,
};
use super::run_task;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected plugin ls|import|export|rm|install".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox plugin ls [container] [--profile <name>]");
        println!("dshbox plugin import <source>");
        println!("dshbox plugin export <id> <dest.tar.gz>");
        println!("dshbox plugin rm <id>");
        println!("dshbox plugin install <container> <source> [--profile <name>]");
        return Ok(());
    }
    match action {
        "ls" | "list" if arguments.len() >= 2 => container_plugins(&arguments[1..]),
        "ls" | "list" => repository_list(),
        "import" => repository_import(arguments.get(1).ok_or(
            "expected a source: GitHub URL, local directory, or tarball",
        )?),
        "export" => repository_export(
            arguments.get(1).ok_or("expected a repository entry id")?,
            arguments.get(2).ok_or("expected a destination path")?,
        ),
        "rm" => repository_remove(arguments.get(1).ok_or("expected a repository entry id")?),
        "install" | "add" => container_plugin_add(&arguments[1..]),
        _ => Err(format!("unknown plugin action: {action}")),
    }
}

fn repository_list() -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    println!("ID\tKIND\tNAME\tVERSION");
    for entry in scan_repository(Path::new(&root)) {
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

fn kind_name(kind: &ExtensionKind) -> &'static str {
    match kind {
        ExtensionKind::Plugin => "plugin",
        ExtensionKind::Skill => "skill",
    }
}

fn repository_import(source: &str) -> Result<(), String> {
    let request = ImportRepositoryExtensionRequest {
        source: source.to_owned(),
    };
    run_task(
        "repository-extension-import",
        vec!["repository:extensions".to_owned()],
        json!({ "source": source }),
        move |task| import_repository_extension(request, task),
    )?;
    println!("imported repository entry from {source}");
    Ok(())
}

fn repository_export(id: &str, destination: &str) -> Result<(), String> {
    let request = ExportRepositoryExtensionRequest {
        repository_id: id.to_owned(),
        destination: destination.to_owned(),
    };
    run_task(
        "repository-extension-export",
        vec!["repository:extensions".to_owned()],
        json!({ "repositoryId": id, "destination": destination }),
        move |task| export_repository_extension(request, task),
    )?;
    println!("exported repository entry {id} to {destination}");
    Ok(())
}

fn repository_remove(id: &str) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let mut entries = scan_repository(Path::new(&root));
    let entry = entries
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
        .ok_or("repository extension not found")?;
    fs::remove_dir_all(PathBuf::from(&entry.source_path).parent().ok_or(
        "repository source has no parent",
    )?)
    .map_err(|error| error.to_string())?;
    entries.retain(|entry| entry.id != id);
    write_repository_index(Path::new(&root), &entries)?;
    println!("removed repository entry {id}");
    Ok(())
}

fn container_plugins(arguments: &[String]) -> Result<(), String> {
    let id = arguments.first().ok_or("expected container id")?;
    let profile = selected_profile(arguments);
    let (_root, directory, _version) = find_container(id)?;
    let manifest = directory
        .join("profile/profiles")
        .join(&profile)
        .join("package.json");
    let value: Value = serde_json::from_str(
        &fs::read_to_string(&manifest)
            .map_err(|_| format!("profile not found: {}", manifest.display()))?,
    )
    .map_err(|error| error.to_string())?;
    for bundle in value
        .pointer("/dsh/profile/bundles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        println!("{bundle}");
    }
    Ok(())
}

fn container_plugin_add(arguments: &[String]) -> Result<(), String> {
    let id = arguments.first().ok_or("expected container id")?;
    let spec = arguments.get(1).ok_or("expected package or URL")?;
    let profile = selected_profile(arguments);
    let (root, directory, version) = find_container(id)?;
    let runtime = runtime_root()?;
    let node = runtime.join(if cfg!(windows) {
        "node/node.exe"
    } else {
        "node/bin/node"
    });
    let pnpm = runtime.join("pnpm/node_modules/pnpm/bin/pnpm.cjs");
    let source = version_directory(&root, &version);
    let mut command = Command::new(node);
    suppress_console_window(&mut command);
    let status = command
        .arg(pnpm)
        .args([
            "--dir",
            source.to_string_lossy().as_ref(),
            "dsh",
            "plugin",
            "--profile",
            &profile,
            "add",
            spec,
        ])
        .env("DSH_HOME", directory.join("profile"))
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        println!("installed {spec} into {id} (profile {profile})");
        Ok(())
    } else {
        Err(format!("dsh plugin add failed with {status}"))
    }
}

fn runtime_root() -> Result<PathBuf, String> {
    let target = match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        _ => return Err("unsupported platform".to_owned()),
    };
    let mut candidates = Vec::new();
    if let Ok(root) = env::var("DSHBOX_RESOURCE_DIR") {
        candidates.push(PathBuf::from(root));
    }
    if let Ok(executable) = env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join("../lib/dshbox"));
            candidates.push(directory.join("resources"));
        }
    }
    candidates.push(PathBuf::from("src-tauri/resources"));
    candidates
        .into_iter()
        .map(|root| root.join("runtime").join(target))
        .find(|path| path.join("runtime-manifest.json").is_file())
        .ok_or("bundled runtime is unavailable; reinstall dshbox".to_owned())
}

fn selected_profile(arguments: &[String]) -> String {
    arguments
        .windows(2)
        .find(|pair| pair[0] == "--profile")
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| "web".to_owned())
}

fn find_container(id: &str) -> Result<(String, PathBuf, String), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = box_containers::scan_containers(&root)?
        .remove(id)
        .ok_or(format!("container not found: {id}"))?;
    Ok((root, PathBuf::from(container.directory), container.version))
}
