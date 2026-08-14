//! `dshbox dsh` — manage installed DeepSeek Harness runtimes.

use box_dsh_versions::{installed_versions, version_directory};
use box_foundation::{read_config, write_config};
use std::fs;

use crate::desktop::app::{install_dsh_version_with_cancel, read_dsh_catalog};

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(
            "expected `dsh ls`, `dsh search`, `dsh install <tag>`, `dsh rm <tag>`, or `dsh refresh`"
                .to_owned(),
        );
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox dsh ls|search|refresh|install <tag>|rm <tag>");
        return Ok(());
    }
    match action {
        "ls" => list_versions(),
        "search" => search_versions(),
        "refresh" => refresh_catalog(),
        "install" => install_version(arguments.get(1).ok_or("expected a DSH version tag")?),
        "rm" | "uninstall" => remove_version(arguments.get(1).ok_or("expected a DSH version tag")?),
        _ => Err(format!("unknown dsh action: {action}")),
    }
}

fn list_versions() -> Result<(), String> {
    let root = runtime_root()?;
    let installed = installed_versions(&root)?;
    let mut names = read_dsh_catalog(&root);
    for name in &installed {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    println!("NAME\tINSTALLED");
    for name in names {
        println!(
            "{}\t{}",
            name,
            if installed.contains(&name) { "yes" } else { "no" }
        );
    }
    Ok(())
}

fn search_versions() -> Result<(), String> {
    let root = runtime_root()?;
    let catalog = read_dsh_catalog(&root);
    if catalog.is_empty() {
        println!("catalog is empty; run `dshbox dsh refresh` to fetch remote versions");
    }
    for name in catalog {
        println!("{name}");
    }
    Ok(())
}

fn refresh_catalog() -> Result<(), String> {
    crate::desktop::app::refresh_dsh_catalog()?;
    println!("DSH version catalog refreshed");
    Ok(())
}

fn install_version(tag: &str) -> Result<(), String> {
    if !is_safe_tag(tag) {
        return Err(format!("invalid DSH version: {tag}"));
    }
    println!("cloning DeepSeek Harness @ {tag} (this may take a while)...");
    let config = install_dsh_version_with_cancel(tag.to_owned(), || false)?;
    println!(
        "installed DSH version {tag} (selected: {})",
        config.selected_dsh_version.as_deref().unwrap_or("-")
    );
    Ok(())
}

fn remove_version(tag: &str) -> Result<(), String> {
    if !is_safe_tag(tag) {
        return Err(format!("invalid DSH version: {tag}"));
    }
    let mut config = read_config()?;
    let root = config
        .runtime_directory
        .as_deref()
        .ok_or("DSH Box storage is not configured")?;
    let directory = version_directory(root, tag);
    if !directory.is_dir() {
        return Err(format!("DSH version is not installed: {tag}"));
    }
    fs::remove_dir_all(&directory)
        .map_err(|error| format!("cannot remove {}: {error}", directory.display()))?;
    if config.selected_dsh_version.as_deref() == Some(tag) {
        config.selected_dsh_version = None;
    }
    write_config(&config)?;
    println!("uninstalled DSH version {tag}");
    Ok(())
}

fn runtime_root() -> Result<String, String> {
    read_config()?
        .runtime_directory
        .ok_or_else(|| "DSH Box storage is not configured".to_owned())
}

fn is_safe_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}
