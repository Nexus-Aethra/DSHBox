//! `dshbox build [path]` — build a BUILT TEMPLATE from a boxfile (or
//! `.dsh` script). The product is metadata only (docs/specs/image-build.md):
//! plugins are referenced from the shared repository, every other kind is
//! snapshotted into the data store — all registered in the single template
//! store. Use `dshbox run <template>` afterwards to create and start a
//! container. The default filename lookup mirrors `Dockerfile` conventions:
//! `./boxfile` → `./Boxfile` → `./.boxfile` in the current working
//! directory.

use std::path::PathBuf;

use box_scheduler::TaskRecord;

use super::flag_value;
use super::rpc;

/// Default filenames searched when no path argument is given.
const DEFAULT_BOXFILE_NAMES: &[&str] = &["boxfile", "Boxfile", ".boxfile"];

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    if matches!(
        arguments.first().map(String::as_str),
        Some("help" | "--help" | "-h")
    ) {
        print_help();
        return Ok(());
    }
    let positional = arguments
        .iter()
        .find(|argument| !argument.starts_with("--"))
        .cloned();
    let file_override = flag_value(arguments, "--file", "");
    let script_path = if !file_override.is_empty() {
        file_override
    } else {
        match positional {
            Some(path) => path,
            None => find_default_boxfile()?,
        }
    };
    let output_path = flag_value(arguments, "--output", "");
    let template_name = flag_value(arguments, "--name", "");
    enqueue(&script_path, opt(&output_path), opt(&template_name))
}

/// Enqueue the build on the daemon and wait for it. `--name` names the
/// built template (default: the boxfile's NAME line); `--output`
/// additionally exports a portable `.dshimage` archive.
pub(crate) fn enqueue(
    script_path: &str,
    output_path: Option<String>,
    template_name: Option<String>,
) -> Result<(), String> {
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
            "containerName": template_name,
        }),
    )?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    rpc::wait_task(&client, &task.id)?;
    println!("built template from {script_path}");
    Ok(())
}

fn find_default_boxfile() -> Result<String, String> {
    for name in DEFAULT_BOXFILE_NAMES {
        let path = PathBuf::from(name);
        if path.is_file() {
            return Ok(path.to_string_lossy().into_owned());
        }
    }
    Err(format!(
        "no boxfile found in current directory (looked for {}); specify a path or pass --file",
        DEFAULT_BOXFILE_NAMES.join(", ")
    ))
}

fn opt(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn print_help() {
    println!("dshbox build [path] [--file <path>] [--name <template>] [--output <path.dshimage>]");
    println!();
    println!("Parse a boxfile and build a TEMPLATE from it (no container is created).");
    println!("Plugins are referenced from the shared repository; every other kind");
    println!("is snapshotted into the data store (docs/specs/image-build.md).");
    println!();
    println!("If no path is given, the command looks for the first match among:");
    println!("  {}", DEFAULT_BOXFILE_NAMES.join(", "));
    println!();
    println!("Boxfile format is the same as .dsh (FROM / PROFILE / NAME / ADD lines).");
    println!("Use 'dshbox run <template>' afterwards to create and start a container.");
}
