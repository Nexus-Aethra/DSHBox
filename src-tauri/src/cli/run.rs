//! `dshbox run <template>` — create a container from a local template and
//! start it through the daemon, mirroring the UI's create-then-start flow.
//! Both template forms work: a BUILT template (product of `dshbox build`)
//! materialises by linking repository references and hard-copying data
//! snapshots; a source script template is parsed and materialised live.

use box_scheduler::TaskRecord;
use serde_json::json;

use super::{flag_value, rpc};

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    if arguments.is_empty() {
        return Err("expected a template name".to_owned());
    }
    if matches!(arguments[0].as_str(), "help" | "--help" | "-h") {
        println!("dshbox run <template> [--name <name>] [--profile <name>]");
        println!();
        println!("Create a container from a local template (built or script) and start it.");
        return Ok(());
    }
    let template = arguments[0].clone();
    let name = flag_value(arguments, "--name", &template);
    let profile = flag_value(arguments, "--profile", "");
    let profile = if profile.is_empty() {
        None
    } else {
        Some(profile)
    };
    // Materialize + start happens inside one daemon task so progress and
    // log lines match the UI's task panel exactly. The daemon resolves
    // whether the template is built (resource list) or a source script.
    let client = rpc::connect()?;
    let value = rpc::call(
        &client,
        "create_container_from_template",
        json!({
            "name": name.clone(),
            "template": template,
            "profile": profile,
        }),
    )?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    rpc::wait_task(&client, &task.id)?;
    // The daemon generates the container id; resolve it and its URL after
    // the task settles.
    let containers_value = rpc::call(&client, "list_containers", json!({}))?;
    let containers: Vec<box_containers::DshContainer> = serde_json::from_value(containers_value)
        .map_err(|error| format!("invalid container list from daemon: {error}"))?;
    let container = containers
        .into_iter()
        .find(|container| container.name == name)
        .ok_or("container not found after create")?;
    let url_value = rpc::call(&client, "container_url", json!({ "id": container.id }))?;
    let url = url_value["url"].as_str().unwrap_or("?");
    println!("container {} started at {url}", container.id);
    Ok(())
}
