use box_containers::scan_containers;
use box_dsh_versions::version_directory;
use box_foundation::read_config;
use serde_json::Value;
use std::{env, fs, path::PathBuf, process::Command};

pub fn run() -> Option<i32> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() || arguments.len() == 1 && arguments[0] == "ui" {
        return None;
    }
    let result = match arguments[0].as_str() {
        "ps" => print_containers(),
        "plugin" => plugin_command(&arguments[1..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        command => Err(format!("unknown command: {command}")),
    };
    if let Err(error) = result {
        eprintln!("dshbox: {error}");
        return Some(1);
    }
    Some(0)
}

fn print_help() {
    println!("dshbox [ui]\ndshbox ps\ndshbox plugin list <container> [--profile <name>]\ndshbox plugin add <container> <package-or-url> [--profile <name>]");
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
    let container = scan_containers(&root)?
        .remove(id)
        .ok_or(format!("container not found: {id}"))?;
    Ok((root, PathBuf::from(container.directory), container.version))
}

fn print_containers() -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    println!("ID\tNAME\tVERSION\tSTATUS");
    for container in scan_containers(&root)?.into_values() {
        println!(
            "{}\t{}\t{}\t{}",
            container.id, container.name, container.version, container.status
        );
    }
    Ok(())
}

fn plugin_command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected plugin list or plugin add".to_owned());
    };
    let id = arguments.get(1).ok_or("expected container id")?;
    let profile = selected_profile(arguments);
    let (root, directory, version) = find_container(id)?;
    match action {
        "list" => {
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
        "add" => {
            let spec = arguments.get(2).ok_or("expected package or URL")?;
            let runtime = runtime_root()?;
            let node = runtime.join(if cfg!(windows) {
                "node/node.exe"
            } else {
                "node/bin/node"
            });
            let pnpm = runtime.join("pnpm/node_modules/pnpm/bin/pnpm.cjs");
            let source = version_directory(&root, &version);
            let status = Command::new(node)
                .arg(pnpm)
                .args([
                    "--dir",
                    source.to_string_lossy().as_ref(),
                    "dsh",
                    "plugin",
                    "add",
                    spec,
                    "--profile",
                    &profile,
                ])
                .env("DSH_HOME", directory.join("profile"))
                .status()
                .map_err(|error| error.to_string())?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("dsh plugin add failed with {status}"))
            }
        }
        _ => Err(format!("unknown plugin action: {action}")),
    }
}
