//! `dshbox container` — describe and control the lifecycle of one container.
//!
//! Every action is a thin RPC against the daemon (the daemon owns the
//! container registry and the DSH host process). `logs` reads the host's
//! stdout/stderr captured during the last start attempt; `url` resolves the
//! webview URL once the host is up; `start`/`stop`/`rebuild` enqueue the
//! corresponding daemon task.

use box_scheduler::TaskRecord;
use serde_json::json;

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected container logs|url|start|stop|rebuild".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox container logs <id>          tail the DSH host log");
        println!("dshbox container url <id>           print the webview URL of a running container");
        println!("dshbox container start <id>         start the DSH host of a stopped container");
        println!("dshbox container stop <id>          stop a running container");
        println!("dshbox container rebuild <id>       re-materialise extensions and restart");
        return Ok(());
    }
    let id = arguments
        .get(1)
        .ok_or_else(|| format!("expected a container id after `container {action}`"))?
        .clone();
    match action {
        "logs" => logs(&id),
        "url" => url(&id),
        "start" => start(&id),
        "stop" => stop(&id),
        "rebuild" => rebuild(&id),
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
