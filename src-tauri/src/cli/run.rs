//! `dshbox run <image|template>` — create a container from a local image
//! (preferred) or template, then start it through the daemon, mirroring
//! the UI's create-then-start flow.

use box_image::registry::ImageEntry;
use box_scheduler::TaskRecord;
use serde_json::json;

use super::{flag_value, rpc};

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    if arguments.is_empty() {
        return Err("expected an image or template name".to_owned());
    }
    if matches!(arguments[0].as_str(), "help" | "--help" | "-h") {
        println!("dshbox run <image|template> [--name <name>] [--profile <name>]");
        println!();
        println!("Create a container from a local image (registry) or template and");
        println!("start it. A name matching a built image wins over a template.");
        return Ok(());
    }
    let base = arguments[0].clone();
    let name = flag_value(arguments, "--name", &base);
    let profile = flag_value(arguments, "--profile", "");
    let profile = if profile.is_empty() {
        None
    } else {
        Some(profile)
    };
    let client = rpc::connect()?;
    // Image-first resolution: a built image of this name shadows any
    // template with the same name.
    let images_value = rpc::call(&client, "list_images", json!({}))?;
    let images: Vec<ImageEntry> = serde_json::from_value(images_value)
        .map_err(|error| format!("invalid image list from daemon: {error}"))?;
    let value = if images.iter().any(|entry| entry.name == base) {
        rpc::call(
            &client,
            "create_container_from_image",
            json!({
                "name": name.clone(),
                "image": base,
            }),
        )?
    } else {
        // Materialize + start happens inside one daemon task so progress
        // and log lines match the UI's task panel exactly.
        rpc::call(
            &client,
            "create_container_from_template",
            json!({
                "name": name.clone(),
                "template": base,
                "profile": profile,
            }),
        )?
    };
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
