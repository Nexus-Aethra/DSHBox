//! `dshbox container` — describe and control the lifecycle of one container.
//!
//! Every action is a thin RPC against the daemon (the daemon owns the
//! container registry and the DSH host process). `logs` reads the host's
//! stdout/stderr captured during the last start attempt; `url` resolves the
//! webview URL once the host is up; `start`/`stop`/`rebuild` enqueue the
//! corresponding daemon task; `describe` dumps the full state (status, URL,
//! host PID, profiles/plugins/skills scan); `open` launches the system
//! browser at the URL; `rm` stops and deletes the container.

use box_api::ContainerDescription;
use box_scheduler::TaskRecord;
use serde_json::{json, Value};

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected container logs|url|describe|show|open|start|stop|rebuild|rm".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox container logs <id>             tail the DSH host log");
        println!("dshbox container url <id>              print the webview URL of a running container");
        println!("dshbox container describe <id> [--json]   print detailed container info");
        println!("dshbox container show <id> [--json]    alias for describe");
        println!("dshbox container open <id>             open the running container in the system browser");
        println!("dshbox container start <id>            start the DSH host of a stopped container");
        println!("dshbox container stop <id>             stop a running container");
        println!("dshbox container rebuild <id>          re-materialise extensions and restart");
        println!("dshbox container rm <id>               stop and delete the container");
        return Ok(());
    }
    let id = arguments
        .get(1)
        .ok_or_else(|| format!("expected a container id after `container {action}`"))?
        .clone();
    match action {
        "logs" => logs(&id),
        "url" => url(&id),
        "describe" | "show" => describe(&id, &arguments[2..]),
        "open" => open(&id),
        "start" => start(&id),
        "stop" => stop(&id),
        "rebuild" => rebuild(&id),
        "rm" | "remove" => remove(&id),
        other => Err(format!("unknown container action: {other}")),
    }
}

fn logs(id: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let containers_value = rpc::call(&client, "list_containers", json!({}))?;
    let containers: Vec<box_containers::DshContainer> =
        serde_json::from_value(containers_value)
            .map_err(|error| format!("invalid container list from daemon: {error}"))?;
    let container = containers
        .into_iter()
        .find(|container| container.id == id)
        .ok_or_else(|| format!("container not found: {id}"))?;
    let log_path = std::path::PathBuf::from(&container.directory)
        .join("logs")
        .join("host.log");
    let text = std::fs::read_to_string(&log_path)
        .map_err(|error| format!("cannot read {}: {error}", log_path.display()))?;
    print!("{text}");
    Ok(())
}

fn url(id: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "container_url", json!({ "id": id }))?;
    let url = value["url"].as_str().unwrap_or("?");
    println!("{url}");
    Ok(())
}

/// Print every field the daemon knows about one container. Two output
/// modes: `--json` for scripts (the full daemon payload, pretty-printed)
/// and the default text view for humans, which walks the extensions
/// profile/skill tree inline.
fn describe(id: &str, rest: &[String]) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "describe_container", json!({ "id": id }))?;
    if rest.iter().any(|flag| flag == "--json") {
        // Re-parse through ContainerDescription so we render the same
        // shape the daemon serialised (camelCase keys, omitted Optional
        // fields), and pretty-print with two-space indent like the rest
        // of the CLI's `--json` outputs.
        let parsed: ContainerDescription = serde_json::from_value(value)
            .map_err(|error| format!("invalid describe_container response: {error}"))?;
        println!(
            "{}",
            serde_json::to_string_pretty(&parsed)
                .map_err(|error| format!("cannot serialize description: {error}"))?
        );
        return Ok(());
    }
    print_description_text(&value);
    Ok(())
}

fn print_description_text(value: &Value) {
    let id = value["id"].as_str().unwrap_or("?");
    let name = value["name"].as_str().unwrap_or("?");
    let version = value["version"].as_str().unwrap_or("?");
    let profile = value["profile"].as_str().unwrap_or("?");
    let template = value["template"].as_str();
    let directory = value["directory"].as_str().unwrap_or("?");
    let status = value["status"].as_str().unwrap_or("stopped");
    let url = value["url"].as_str();
    let host_pid = value["hostPid"].as_u64();
    println!("id:        {id}");
    println!("name:      {name}");
    println!("version:   {version}");
    println!("profile:   {profile}");
    match template {
        Some(value) => println!("template:  {value}"),
        None => println!("template:  -"),
    }
    println!("status:    {status}");
    match url {
        Some(value) => println!("url:       {value}"),
        None => println!("url:       -"),
    }
    match host_pid {
        Some(value) => println!("pid:       {value}"),
        None => println!("pid:       -"),
    }
    println!("directory: {directory}");
    println!();
    println!("profiles:");
    if let Some(profiles) = value["extensions"]["profiles"].as_array() {
        if profiles.is_empty() {
            println!("  -");
        } else {
            for profile in profiles {
                let name = profile["name"].as_str().unwrap_or("?");
                println!("  {name}");
                if let Some(plugins) = profile["plugins"].as_array() {
                    if plugins.is_empty() {
                        println!("    plugins: -");
                    } else {
                        println!("    plugins:");
                        for plugin in plugins {
                            let plugin_name = plugin["name"].as_str().unwrap_or("?");
                            let plugin_version = plugin["version"].as_str().unwrap_or("-");
                            let diagnostic = plugin["diagnostic"].as_str();
                            match diagnostic {
                                Some(error) => {
                                    println!("      - {plugin_name} {plugin_version} (error: {error})")
                                }
                                None => println!("      - {plugin_name} {plugin_version}"),
                            }
                        }
                    }
                }
                if let Some(diagnostics) = profile["diagnostics"].as_array() {
                    if !diagnostics.is_empty() {
                        println!("    diagnostics:");
                        for line in diagnostics {
                            if let Some(text) = line.as_str() {
                                println!("      - {text}");
                            }
                        }
                    }
                }
            }
        }
    } else {
        println!("  -");
    }
    println!("skills:");
    if let Some(skills) = value["extensions"]["skills"].as_array() {
        if skills.is_empty() {
            println!("  -");
        } else {
            for skill in skills {
                let skill_name = skill["name"].as_str().unwrap_or("?");
                let skill_path = skill["path"].as_str().unwrap_or("-");
                println!("  - {skill_name} ({skill_path})");
            }
        }
    } else {
        println!("  -");
    }
    if let Some(diagnostics) = value["extensions"]["diagnostics"].as_array() {
        if !diagnostics.is_empty() {
            println!("diagnostics:");
            for line in diagnostics {
                if let Some(text) = line.as_str() {
                    println!("  - {text}");
                }
            }
        }
    }
    if let Some(scanned_at) = value["extensions"]["scannedAt"].as_u64() {
        println!("scanned_at: {scanned_at}");
    }
}

/// Open the running container in the system browser. Uses the same
/// `webbrowser` crate as the desktop's `open_dsh_front_browser` Tauri
/// command — both paths must work on Linux/macOS/Windows.
fn open(id: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "container_url", json!({ "id": id }))?;
    let url = value["url"]
        .as_str()
        .ok_or_else(|| format!("container is not running: {id}"))?
        .to_owned();
    webbrowser::open(&url).map_err(|error| format!("cannot open system browser: {error}"))?;
    println!("opened {url}");
    Ok(())
}

fn start(id: &str) -> Result<(), String> {
    enqueue_lifecycle(id, "enqueue_container_start", "container-start")
}

fn stop(id: &str) -> Result<(), String> {
    enqueue_lifecycle(id, "enqueue_container_stop", "container-stop")
}

fn rebuild(id: &str) -> Result<(), String> {
    enqueue_lifecycle(id, "enqueue_container_rebuild", "container-rebuild")
}

fn enqueue_lifecycle(id: &str, method: &str, kind: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, method, json!({ "id": id }))?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    rpc::wait_task(&client, &task.id)?;
    println!("{kind} for container {id} finished");
    Ok(())
}

/// `rm` / `remove` stops the host first (so its PID is gone and the
/// reference counts for repository-backed plugins can be released), then
/// deletes the container directory. Both steps happen inside the daemon's
/// `delete_container` RPC — the CLI never deletes anything itself.
fn remove(id: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "delete_container", json!({ "id": id }))?;
    let deleted = value["deleted"].as_bool().unwrap_or(false);
    if deleted {
        println!("deleted container {id}");
    } else {
        println!("container {id}: daemon reported no deletion");
    }
    Ok(())
}
