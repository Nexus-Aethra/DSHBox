//! `dshbox container workspace` — list and register the directories a
//! container opens its sessions in.
//!
//! The UI adds a workspace through a native directory picker, and a native
//! picker is not reachable from a terminal, a script or an agent: it opens on
//! the host desktop, outside the app, and the only way through it is a hand.
//! That is the whole reason this verb exists. The registry underneath is one
//! JSON document the UI also writes, so a workspace added here shows up in its
//! picker and one added there is visible here.
//!
//! Writing refuses a running container: the host holds that document in memory
//! and rewrites it, so a change made underneath it disappears. `--restart`
//! stops, writes and starts again, in that order.

use serde_json::json;

use super::rpc;

const HELP: &str = "\
dshbox container workspace list <id> [--json]
        the workspaces this container would show in its picker
dshbox container workspace add <id> --path <dir> [options] [--json]
        --title <t>   name for it (default: the directory's own name)
        --restart     stop the container, add, then start it again
        --path is absolute and must name an existing directory. A relative
        path is refused rather than resolved: the container is not your shell,
        so its working directory is not a base anything could mean.

Every verb takes --json. Adding a path that is already registered reports it
as already registered instead of failing, so a caller can set this up without
checking first.";
pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected workspace list|add".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let rest = &arguments[1..];
    let id = rest
        .first()
        .map(String::as_str)
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .ok_or_else(|| format!("expected a container id after `container workspace {action}`"))?;
    let flags = Flags::parse(&rest[1..])?;
    match action {
        "list" | "ls" => list(id, &flags),
        "add" => add(id, &flags),
        other => Err(format!("unknown workspace action: {other}")),
    }
}

/// A tiny flag reader: `--flag value`, `--flag=value`, and bare `--switch`es.
struct Flags {
    values: Vec<(String, String)>,
    switches: Vec<String>,
}

impl Flags {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut values = Vec::new();
        let mut switches = Vec::new();
        let mut index = 0;
        while index < arguments.len() {
            let Some(name) = arguments[index].strip_prefix("--") else {
                return Err(format!("unexpected argument: {}", arguments[index]));
            };
            if let Some((name, value)) = name.split_once('=') {
                values.push((name.to_owned(), value.to_owned()));
                index += 1;
                continue;
            }
            if matches!(name, "json" | "restart") {
                switches.push(name.to_owned());
                index += 1;
                continue;
            }
            let value = arguments
                .get(index + 1)
                .ok_or_else(|| format!("--{name} needs a value"))?
                .clone();
            values.push((name.to_owned(), value));
            index += 2;
        }
        Ok(Self { values, switches })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|switch| switch == name)
    }
}

fn list(id: &str, flags: &Flags) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_container_workspaces", json!({ "id": id }))?;
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return Ok(());
    }
    let rows = value["workspaces"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("container {id} has no workspaces yet; add one with `dshbox container workspace add {id} --path <dir>`");
        return Ok(());
    }
    println!("{:<38} {:<26} {:>9}  {}", "id", "title", "sessions", "path");
    for row in rows {
        let unlisted = if row["listed"].as_bool().unwrap_or(true) {
            ""
        } else {
            "  (not in the display list)"
        };
        println!(
            "{:<38} {:<26} {:>9}  {}{}",
            row["id"].as_str().unwrap_or("?"),
            row["title"].as_str().unwrap_or("?"),
            row["sessions"].as_u64().unwrap_or(0),
            row["path"].as_str().unwrap_or("?"),
            unlisted,
        );
    }
    Ok(())
}
fn add(id: &str, flags: &Flags) -> Result<(), String> {
    let path = flags
        .get("path")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("expected --path <dir> for `container workspace add {id}`"))?
        .to_owned();
    let mut request = json!({ "id": id, "path": path });
    if let Some(title) = flags.get("title") {
        request["title"] = json!(title);
    }
    let client = rpc::connect()?;
    if flags.has("restart") {
        // Stop, write, start -- in that order, and each step's own error
        // reported. Starting first would have the host rewrite the registry
        // over the top of what was just added.
        rpc::run_task(&client, "enqueue_container_stop", json!({ "id": id }))?;
    }
    let value = rpc::call(&client, "add_container_workspace", request.clone())?;
    if flags.has("restart") {
        rpc::run_task(&client, "enqueue_container_start", json!({ "id": id }))?;
    }
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return Ok(());
    }
    let verb = if value["created"].as_bool().unwrap_or(false) {
        "Registered"
    } else {
        "Already registered"
    };
    println!(
        "{verb} the workspace {} at {}.",
        value["title"].as_str().unwrap_or("?"),
        value["path"].as_str().unwrap_or(&path),
    );
    Ok(())
}
