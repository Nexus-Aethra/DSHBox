//! `dshbox build [path]` — build an IMAGE from a boxfile (or `.dsh`
//! script). The image is metadata only (docs/specs/image-build.md); use
//! `dshbox run <image>` afterwards to create and start a container.
//! The default filename lookup mirrors `Dockerfile` conventions:
//! `./boxfile` → `./Boxfile` → `./.boxfile` in the current working
//! directory.

use std::path::PathBuf;

use super::flag_value;
use super::image;

/// Default filenames searched when no path argument is given.
const DEFAULT_BOXFILE_NAMES: &[&str] = &["boxfile", "Boxfile", ".boxfile"];

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    if matches!(arguments.first().map(String::as_str), Some("help" | "--help" | "-h")) {
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
    let container_name = flag_value(arguments, "--name", "");
    image::build(
        &script_path,
        opt(&output_path),
        opt(&container_name),
    )
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
    if value.is_empty() { None } else { Some(value.to_owned()) }
}

fn print_help() {
    println!("dshbox build [path] [--file <path>] [--name <image>] [--output <path.dshimage>]");
    println!();
    println!("Parse a boxfile and build an IMAGE from it (no container is created).");
    println!("Plugins are referenced from the shared repository; every other kind");
    println!("is snapshotted into the data store (docs/specs/image-build.md).");
    println!();
    println!("If no path is given, the command looks for the first match among:");
    println!("  {}", DEFAULT_BOXFILE_NAMES.join(", "));
    println!();
    println!("Boxfile format is the same as .dsh (FROM / PROFILE / NAME / ADD lines).");
    println!("Use 'dshbox run <image>' afterwards to create and start a container.");
}
