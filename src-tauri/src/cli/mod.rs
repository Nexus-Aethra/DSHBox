//! Command-line interface for DSH Box.
//!
//! No arguments launches the desktop UI; every other invocation is a command.
//! Long-running commands reuse the same `box_scheduler` task machinery as the
//! UI task queue, so progress, log, and cancel semantics stay identical.

pub mod bundle;
pub mod config;
pub mod dsh;
pub mod plugin;

use box_dsh_versions::installed_versions;
use box_extensions::{read_bundles, scan_repository};
use box_foundation::{read_config, BoxPaths};
use box_scheduler::{run_queued, TaskContext, TaskManager, TaskNotifier};
use serde_json::Value;
use std::{env, path::Path, sync::Arc};

pub fn run() -> Option<i32> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty()
        || arguments.len() == 1 && matches!(arguments[0].as_str(), "ui" | "--tray")
    {
        return None;
    }
    let result = match arguments[0].as_str() {
        "--version" | "-V" => {
            println!("dshbox {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "ps" => print_containers(),
        "info" => print_info(),
        "dsh" => dsh::command(&arguments[1..]),
        "plugin" => plugin::command(&arguments[1..]),
        "bundle" => bundle::command(&arguments[1..]),
        "config" => config::command(&arguments[1..]),
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

fn print_containers() -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    println!("ID\tNAME\tVERSION\tSTATUS");
    for container in box_containers::scan_containers(&root)?.into_values() {
        println!(
            "{}\t{}\t{}\t{}",
            container.id, container.name, container.version, container.status
        );
    }
    Ok(())
}

fn print_info() -> Result<(), String> {
    let config = read_config()?;
    println!("DSH Box {}", env!("CARGO_PKG_VERSION"));
    match &config.runtime_directory {
        Some(root) => {
            println!("runtime directory: {root}");
            println!("installed DSH versions: {}", installed_versions(root)?.len());
            println!(
                "containers: {}",
                box_containers::scan_containers(root)?.len()
            );
            println!("repository entries: {}", scan_repository(Path::new(root)).len());
            println!("bundles: {}", read_bundles(Path::new(root)).len());
            println!(
                "github mirror: {}",
                config.github_mirror.as_deref().unwrap_or("-")
            );
            println!("npm registry: {}", config.npm_registry.as_deref().unwrap_or("-"));
        }
        None => println!("runtime directory: not configured"),
    }
    Ok(())
}

fn print_help() {
    println!("dshbox [command] [options]");
    println!();
    println!("No command launches the desktop UI. The CLI shares the same task");
    println!("queue as the UI; long operations print progress to stderr.");
    println!();
    println!("Global:");
    println!("  dshbox --version            print the DSH Box version");
    println!("  dshbox info                 show storage and resource summary");
    println!("  dshbox ps                   list containers");
    println!("  dshbox help                 print this help");
    println!();
    println!("DSH runtimes:");
    println!("  dshbox dsh ls               list installed DSH versions");
    println!("  dshbox dsh search           list remote DSH versions from the catalog");
    println!("  dshbox dsh refresh          refresh the remote catalog");
    println!("  dshbox dsh install <tag>    clone and install a DSH version");
    println!("  dshbox dsh rm <tag>         uninstall a DSH version");
    println!();
    println!("Plugins and skills:");
    println!("  dshbox plugin ls                        list repository entries");
    println!("  dshbox plugin ls <container> [--profile <name>]");
    println!("  dshbox plugin import <source>           import from GitHub URL, directory, or tarball");
    println!("  dshbox plugin export <id> <dest.tar.gz> export a repository entry");
    println!("  dshbox plugin rm <id>                   remove a repository entry");
    println!("  dshbox plugin install <container> <source> [--profile <name>]");
    println!();
    println!("Bundles:");
    println!("  dshbox bundle ls                        list bundles");
    println!("  dshbox bundle create <name> --plugin <id> [--plugin <id> ...]");
    println!("  dshbox bundle rm <id>                   delete a bundle");
    println!("  dshbox bundle save <id> <dest.tar.gz> [--mode quick|full]");
    println!("  dshbox bundle load <archive> [--conflict keep|overwrite]");
    println!();
    println!("Configuration:");
    println!("  dshbox config show");
    println!("  dshbox config set runtime <dir>");
    println!("  dshbox config set mirror.github <url>");
    println!("  dshbox config set mirror.npm <url>");
}

/// Runs a worker function through the same queued-task machinery as the UI.
/// Progress and log lines go to stderr; the final result is returned after
/// the worker completes, so CLI commands behave like their UI counterparts.
pub(crate) fn run_task(
    kind: &str,
    resource_keys: Vec<String>,
    params: Value,
    work: impl FnOnce(&TaskContext) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let config = read_config()?;
    let paths = BoxPaths::from_config(&config)?;
    let manager = TaskManager::default();
    let task = manager.enqueue(&paths, kind, resource_keys, params)?;
    run_queued(&manager, &paths, Arc::new(CliNotifier), &task.id, work);
    let finished = manager.task(&task.id)?;
    match finished.status.as_str() {
        "succeeded" => Ok(()),
        "cancelled" => Err("task cancelled".to_owned()),
        _ => Err(finished
            .error
            .unwrap_or_else(|| format!("task failed at {}", finished.stage))),
    }
}

/// Progress reporter that forwards scheduler stages and logs to stderr so
/// stdout stays reserved for command output.
pub(crate) struct CliNotifier;

impl TaskNotifier for CliNotifier {
    fn stage(&self, _task_id: &str, stage: &str, progress: u8) {
        eprintln!("[{progress:>3}%] {stage}");
    }

    fn log(&self, _task_id: &str, line: &str) {
        eprintln!("  {line}");
    }
}

/// Reads a `--flag <value>` pair, falling back to the default.
pub(crate) fn flag_value(arguments: &[String], flag: &str, default: &str) -> String {
    arguments
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| default.to_owned())
}
