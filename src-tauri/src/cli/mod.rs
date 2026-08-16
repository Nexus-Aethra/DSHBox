//! Command-line interface for DSH Box.
//!
//! No arguments prints help; `dshbox ui` launches the desktop GUI.
//! Long-running commands reuse the same `box_scheduler` task machinery as the
//! UI task queue, so progress, log, and cancel semantics stay identical.

pub mod build;
pub mod bundle;
pub mod config;
pub mod container;
pub mod image;
pub mod init;
pub mod plugin;
pub mod pull;
pub mod rpc;
pub mod run;
pub mod template;

use serde_json::json;
use std::env;

pub fn run() -> Option<i32> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    // Only launch the GUI when the user explicitly asks for it.
    if arguments.len() == 1 && matches!(arguments[0].as_str(), "ui" | "--tray") {
        return None;
    }
    if arguments.is_empty() {
        print_help();
        return Some(0);
    }
    let result = match arguments[0].as_str() {
        "--version" | "-V" => {
            println!("dshbox {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "ps" => print_containers(),
        "info" => print_info(),
        "pull" => pull::command(&arguments[1..]),
        "plugin" => plugin::command(&arguments[1..]),
        "bundle" => bundle::command(&arguments[1..]),
        "build" => build::command(&arguments[1..]),
        "init" => init::command(&arguments[1..]),
        "run" => run::command(&arguments[1..]),
        "config" => config::command(&arguments[1..]),
        "container" => container::command(&arguments[1..]),
        "image" => image::command(&arguments[1..]),
        "rpc" => raw_rpc(&arguments[1..]),
        "template" => template::command(&arguments[1..]),
        "resources" => {
            eprintln!("warning: 'dshbox resources' is deprecated; 'plugin' and 'bundle' cover its actions.");
            Err("'dshbox resources' has been removed; use 'dshbox plugin ...' or 'dshbox bundle ...'".to_owned())
        }
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

/// Raw RPC escape hatch: `dshbox rpc <method> [json-params]` forwards one
/// request to the daemon verbatim and pretty-prints the response. This is
/// the curl-equivalent debugging tool for the 50+ RPC methods — no typed
/// command needed to poke any endpoint.
fn raw_rpc(arguments: &[String]) -> Result<(), String> {
    let method = arguments
        .first()
        .filter(|value| !value.is_empty())
        .ok_or("usage: dshbox rpc <method> [json-params]")?;
    let params: serde_json::Value = match arguments.get(1) {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|error| format!("invalid JSON params: {error}"))?,
        None => serde_json::json!({}),
    };
    if !params.is_object() {
        return Err("params must be a JSON object".to_owned());
    }
    let client = rpc::connect()?;
    let value = rpc::call(&client, method, params)?;
    println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
    Ok(())
}

fn print_containers() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_containers", json!({}))?;
    let containers: Vec<box_containers::DshContainer> = serde_json::from_value(value)
        .map_err(|error| format!("invalid container list from daemon: {error}"))?;
    println!("ID\tNAME\tVERSION\tSTATUS");
    for container in containers {
        println!(
            "{}\t{}\t{}\t{}",
            container.id, container.name, container.version, container.status
        );
    }
    Ok(())
}

fn print_info() -> Result<(), String> {
    let client = rpc::connect()?;
    let info = rpc::call(&client, "get_info", json!({}))?;
    println!("DSH Box {}", info["version"].as_str().unwrap_or("?"));
    match info["runtimeDirectory"].as_str() {
        Some(root) => {
            println!("runtime directory: {root}");
            println!(
                "installed DSH versions: {}",
                info["dshVersions"].as_u64().unwrap_or(0)
            );
            println!("containers: {}", info["containers"].as_u64().unwrap_or(0));
            println!(
                "repository entries: {}",
                info["repositoryEntries"].as_u64().unwrap_or(0)
            );
            println!("bundles: {}", info["bundles"].as_u64().unwrap_or(0));
            println!(
                "github mirror: {}",
                info["githubMirror"].as_str().unwrap_or("-")
            );
            println!(
                "npm registry: {}",
                info["npmRegistry"].as_str().unwrap_or("-")
            );
        }
        None => println!("runtime directory: not configured"),
    }
    Ok(())
}

fn print_help() {
    println!("dshbox [command] [options]");
    println!();
    println!("DSH Box is a docker-style container runtime. The three-step workflow is");
    println!("pull a base template, build an image from a boxfile, then create a");
    println!("container from the image and run it.");
    println!();
    println!("Quick start (build one image and run it):");
    println!("  1. dshbox init              generate a starter boxfile.dsh in the cwd");
    println!("  2. dshbox pull template github.com/<owner>/<repo>[:tag]");
    println!("     fetch a base template; tag defaults to `:latest` when omitted");
    println!("  3. dshbox build [boxfile.dsh] [--name <container>]");
    println!("     build an image from a boxfile (FROM <template> + ADD instructions);");
    println!("     the boxfile is a `.dsh` script with the same shape as a Dockerfile");
    println!("  4. dshbox run <template> [--name <container>]");
    println!("     create a container from a local template and start it");
    println!();
    println!("Common commands:");
    println!("  dshbox --version           print the DSH Box version");
    println!("  dshbox info                show storage and resource summary");
    println!("  dshbox ps                  list running and stopped containers");
    println!("  dshbox help                print this help");
    println!("  dshbox rpc <method> [json] raw daemon RPC call, pretty-printed (debug)");
    println!("  dshbox ui                  launch the desktop GUI");
    println!();
    println!("Pull:");
    println!("  dshbox pull template <ref>  fetch a template by reference and generate its base .dsh");
    println!("                              (e.g. `github.com/deepseek-ai/deepseek-harness:latest`)");
    println!();
    println!("Build and run:");
    println!("  dshbox init [path] [--force]  write a starter boxfile.dsh to PATH (default ./boxfile.dsh)");
    println!("  dshbox build [path]           build a container from a boxfile (default ./boxfile)");
    println!("  dshbox run <template>         create a container from a local template and start it");
    println!();
    println!("Container lifecycle:");
    println!("  dshbox container logs <id>      tail the DSH host log");
    println!("  dshbox container url <id>       print the webview URL of a running container");
    println!("  dshbox container start <id>     start the DSH host of a stopped container");
    println!("  dshbox container stop <id>      stop a running container");
    println!("  dshbox container rebuild <id>   re-materialise extensions and restart");
    println!();
    println!("Templates:");
    println!("  dshbox template ls                      list local templates");
    println!("  dshbox template show <name>             print the script body of a template");
    println!("  dshbox template import <file.tar.gz>    install a template from a tarball");
    println!("  dshbox template export <name> [dest]    write a template to a tarball");
    println!("  dshbox template rm <name>              remove a template (refuses if in use)");
    println!();
    println!("Plugins and skills:");
    println!("  dshbox plugin <action>     manage repository entries (ls / import / export / rm / prune / install)");
    println!();
    println!("Bundles:");
    println!("  dshbox bundle <action>     manage bundles (ls / create / rm / save / load)");
    println!();
    println!("Configuration:");
    println!("  dshbox config <action>     show or set runtime, mirror, and registry");
    println!();
    println!("Run 'dshbox <command> help' for action-level usage.");
}

/// Reads a `--flag <value>` pair, falling back to the default.
pub(crate) fn flag_value(arguments: &[String], flag: &str, default: &str) -> String {
    arguments
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| default.to_owned())
}
