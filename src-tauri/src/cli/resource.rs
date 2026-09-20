//! `dshbox container resource` — list, extract, inject and drop the state a
//! container persists (chat history, provider credentials, plugin state).
//!
//! Everything is a thin RPC: the daemon owns the store and the container
//! registry, and extraction/injection are background tasks there.

use box_scheduler::TaskRecord;
use serde_json::json;

use super::rpc;

const HELP: &str = "\
dshbox container resource list <id> [--plugin <pkg>] [--json]
        what this container persists: built-in kinds, plugin declarations,
        scanned candidates (kind, location, size, secret)
dshbox container resource stored [--json]
        resources already extracted into the Box store
dshbox container resource extract <id> <kind> [options]
        --entry <e>     one entry only (e.g. a single session); use
                        --entry=<e> for slugs that start with `--`
        --name <n>      name for the extracted resource
        --dest <path>   explicit path for a kind Box does not know
        --plugin <pkg>  resolve the kind through this plugin's declarations
        --out <file>    also pack the payload as a .tar.gz
dshbox container resource inject <id> <resource-id> [options]
dshbox container resource inject <id> --from <container> [options]
dshbox container resource inject <id> --in <file.tar.gz> [options]
        --kind <k>      kind to inject as (default: where it came from)
        --dest <path>   explicit destination inside the container
        --entry <e>     one entry only
        --merge         merge entry by entry (incoming wins)
        --overwrite     replace the kind's path first
        --restart       a running container is stopped, injected, then started
                        again (a stopped container stays stopped)
dshbox container resource rm <resource-id>
        delete an extracted resource and its payload

Injection refuses an existing destination unless --overwrite or --merge is
given, and refuses a running container unless --restart is given.";

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected resource list|stored|extract|inject|rm".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let rest = &arguments[1..];
    match action {
        "list" => list(id_argument("list", rest)?, &rest[1..]),
        "stored" => stored(rest),
        "extract" => extract(rest),
        "inject" => inject(rest),
        "rm" | "remove" => remove(rest),
        other => Err(format!("unknown resource action: {other}")),
    }
}

fn id_argument<'a>(action: &str, arguments: &'a [String]) -> Result<&'a str, String> {
    arguments
        .first()
        .map(String::as_str)
        .filter(|id| !id.is_empty() && !id.starts_with('-'))
        .ok_or_else(|| format!("expected a container id after `container resource {action}`"))
}

/// A tiny flag reader: `--flag value` and `--switch`.
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
            let argument = &arguments[index];
            let Some(name) = argument.strip_prefix("--") else {
                return Err(format!("unexpected argument: {argument}"));
            };
            // `--entry=<value>` exists because entry paths start with `--`
            // themselves (a workspace slug like `--home-wpp--/session-<id>`).
            if let Some((name, value)) = name.split_once('=') {
                values.push((name.to_owned(), value.to_owned()));
                index += 1;
                continue;
            }
            let known_switch = matches!(name, "json" | "merge" | "overwrite" | "restart");
            if known_switch {
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

    fn conflict(&self) -> &'static str {
        if self.has("merge") {
            "merge"
        } else if self.has("overwrite") {
            "overwrite"
        } else {
            "refuse"
        }
    }
}

fn list(id: &str, arguments: &[String]) -> Result<(), String> {
    let flags = Flags::parse(arguments)?;
    let client = rpc::connect()?;
    let mut request = json!({ "id": id });
    if let Some(plugin) = flags.get("plugin") {
        request["plugin"] = json!(plugin);
    }
    let value = rpc::call(&client, "list_container_resources", request)?;
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return Ok(());
    }
    println!("container {id} (profile {})", value["profile"].as_str().unwrap_or("?"));
    println!();
    println!("{:<14} {:<44} {:<9} {:>10}  {}", "kind", "path", "source", "size", "state");
    for resource in value["resources"].as_array().cloned().unwrap_or_default() {
        let kind = resource["kind"]["id"].as_str().unwrap_or("?");
        let path = resource["kind"]["path"].as_str().unwrap_or("?");
        let scope = resource["scope"].as_str().unwrap_or("?");
        let files = resource["files"].as_u64().unwrap_or(0);
        let exists = resource["exists"].as_bool().unwrap_or(false);
        let secret = resource["kind"]["secret"].as_bool().unwrap_or(false);
        let size = human_bytes(resource["bytes"].as_u64().unwrap_or(0));
        let state = match (exists, secret) {
            (true, true) => format!("{files} files · secret"),
            (true, false) => format!("{files} files"),
            (false, true) => "absent · secret".to_owned(),
            (false, false) => "absent".to_owned(),
        };
        println!("{kind:<14} {path:<44} {scope:<9} {size:>10}  {state}");
    }
    let stored = value["stored"].as_array().cloned().unwrap_or_default();
    if !stored.is_empty() {
        println!();
        println!("extracted:");
        for resource in stored {
            println!(
                "  {:<34} {:<12} {:>10}  from {}",
                resource["id"].as_str().unwrap_or("?"),
                resource["kind"].as_str().unwrap_or("?"),
                human_bytes(resource["bytes"].as_u64().unwrap_or(0)),
                resource["sourceContainer"].as_str().unwrap_or("?"),
            );
        }
    }
    Ok(())
}

fn stored(arguments: &[String]) -> Result<(), String> {
    let flags = Flags::parse(arguments)?;
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_resources", json!({}))?;
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return Ok(());
    }
    let resources = value["resources"].as_array().cloned().unwrap_or_default();
    if resources.is_empty() {
        println!("no extracted resources");
        return Ok(());
    }
    for resource in resources {
        println!(
            "{:<34} {:<12} {:>10}  {:>5} files  {}",
            resource["id"].as_str().unwrap_or("?"),
            resource["kind"].as_str().unwrap_or("?"),
            human_bytes(resource["bytes"].as_u64().unwrap_or(0)),
            resource["files"].as_u64().unwrap_or(0),
            if resource["secret"].as_bool().unwrap_or(false) { "secret" } else { "" },
        );
    }
    Ok(())
}

fn extract(arguments: &[String]) -> Result<(), String> {
    let id = id_argument("extract", arguments)?;
    let kind = arguments
        .get(1)
        .filter(|kind| !kind.starts_with('-'))
        .ok_or("expected a resource kind after the container id")?
        .clone();
    let flags = Flags::parse(&arguments[2..])?;
    let client = rpc::connect()?;
    let mut request = json!({ "id": id, "kind": kind });
    for (name, key) in [
        ("entry", "entry"),
        ("name", "name"),
        ("dest", "dest"),
        ("plugin", "plugin"),
        ("out", "out"),
    ] {
        if let Some(value) = flags.get(name) {
            request[key] = json!(value);
        }
    }
    let value = rpc::call(&client, "enqueue_resource_extract", request)?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    rpc::wait_task(&client, &task.id)?;
    println!("extracted {kind} from {id}");
    Ok(())
}

fn inject(arguments: &[String]) -> Result<(), String> {
    let id = id_argument("inject", arguments)?;
    let mut rest = &arguments[1..];
    // An optional positional resource id comes before any flag.
    let mut request = json!({
        "id": id,
        "conflict": "refuse",
    });
    if let Some(positional) = rest.first().filter(|value| !value.starts_with('-')) {
        request["resource"] = json!(positional);
        rest = &rest[1..];
    }
    let flags = Flags::parse(rest)?;
    for (name, key) in [
        ("from", "from"),
        ("in", "input"),
        ("kind", "kind"),
        ("dest", "dest"),
        ("entry", "entry"),
    ] {
        if let Some(value) = flags.get(name) {
            request[key] = json!(value);
        }
    }
    request["conflict"] = json!(flags.conflict());
    if flags.has("restart") {
        request["restart"] = json!(true);
    }
    let client = rpc::connect()?;
    let value = rpc::call(&client, "enqueue_resource_inject", request)?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    rpc::wait_task(&client, &task.id)?;
    println!("injected into {id}");
    Ok(())
}

fn remove(arguments: &[String]) -> Result<(), String> {
    let id = arguments
        .first()
        .filter(|id| !id.starts_with('-'))
        .ok_or("expected a resource id")?;
    let client = rpc::connect()?;
    let value = rpc::call(&client, "delete_resource", json!({ "resourceId": id }))?;
    let _ = value;
    println!("deleted resource {id}");
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
