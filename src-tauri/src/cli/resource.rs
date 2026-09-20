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
dshbox container resource read <id> <path> [--section a.b] [--json]
        one file of a container as a YAML tree: the nodes, their key paths and
        the block at --section (empty = the whole document)
dshbox container resource write <id> <path> [options]
        --section <a.b>  key path to write at (empty = the whole file)
        --text <yaml>    the block that goes *at* that path, without the
                        path's own key (writing at `a.b` a text of `c: 1`
                        produces `a: {b: {c: 1}}`)
        --file <f>       read the block from a file instead
        --merge          merge into that block (default)
        --overwrite      replace that block
        --restart        a running container is stopped, written, then started
dshbox container resource types [--json]
        the resource types Box shows as tabs (kind, container, path, label)
dshbox container resource type rm <view-id>
        remove one of those types (the state itself is untouched)
dshbox container resource rm <resource-id>
        delete an extracted resource and its payload

Injection refuses an existing destination unless --overwrite or --merge is
given, and refuses a running container unless --restart is given.";

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected resource list|stored|types|type|extract|inject|read|write|rm".to_owned());
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
        "types" => types(rest),
        "type" => match rest.first().map(String::as_str) {
            Some("rm" | "remove") => type_remove(&rest[1..]),
            _ => Err("expected resource type rm <view-id>".to_owned()),
        },
        "read" => read(rest),
        "write" => write(rest),
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
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&json!({ "task": task.id, "kind": kind, "container": id })).map_err(|error| error.to_string())?);
    } else {
        println!("extracted {kind} from {id}");
    }
    Ok(())
}

/// The resource types Box shows as tabs, which is what `dshbox apply` registers.
fn types(arguments: &[String]) -> Result<(), String> {
    let flags = Flags::parse(arguments)?;
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_resource_views", json!({}))?;
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?);
        return Ok(());
    }
    let views = value["views"].as_array().cloned().unwrap_or_default();
    if views.is_empty() {
        println!("no resource types; add one with `dshbox apply` or the Box UI");
        return Ok(());
    }
    for view in views {
        println!(
            "{:<24} {:<14} {:<28} {}",
            view["id"].as_str().unwrap_or("?"),
            view["kind"].as_str().unwrap_or("?"),
            view["path"].as_str().unwrap_or("—"),
            view["label"].as_str().unwrap_or("?"),
        );
    }
    Ok(())
}

fn type_remove(arguments: &[String]) -> Result<(), String> {
    let id = arguments
        .first()
        .filter(|id| !id.starts_with('-'))
        .ok_or("expected a resource type id")?
        .clone();
    let flags = Flags::parse(&arguments[1..])?;
    let client = rpc::connect()?;
    let value = rpc::call(&client, "delete_resource_view", json!({ "viewId": id }))?;
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?);
    } else {
        println!("removed resource type {id}");
    }
    Ok(())
}

/// Read a file as a YAML tree, so a block can be named by its key path.
fn read(arguments: &[String]) -> Result<(), String> {
    let id = id_argument("read", arguments)?;
    let path = arguments
        .get(1)
        .filter(|path| !path.starts_with('-'))
        .ok_or("expected a container-relative path after the container id")?
        .clone();
    let flags = Flags::parse(&arguments[2..])?;
    let client = rpc::connect()?;
    let mut request = json!({ "id": id, "path": path });
    if let Some(section) = flags.get("section") {
        request["section"] = json!(section.split('.').filter(|key| !key.is_empty()).collect::<Vec<&str>>());
    }
    let value = rpc::call(&client, "read_resource_tree", request)?;
    if flags.has("json") {
        println!("{}", serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?);
        return Ok(());
    }
    let nodes = value["nodes"].as_array().cloned().unwrap_or_default();
    for node in &nodes {
        let depth = node["depth"].as_u64().unwrap_or(0) as usize;
        let key_path = node["path"]
            .as_array()
            .map(|keys| keys.iter().filter_map(|key| key.as_str()).collect::<Vec<&str>>().join("."))
            .unwrap_or_default();
        println!(
            "{}{} [{}] {}",
            "  ".repeat(depth),
            if key_path.is_empty() { "/" } else { &key_path },
            node["kind"].as_str().unwrap_or("?"),
            node["preview"].as_str().unwrap_or("")
        );
    }
    println!("---");
    print!("{}", value["text"].as_str().unwrap_or(""));
    Ok(())
}

/// Write one YAML block back into a container file, at a key path.
fn write(arguments: &[String]) -> Result<(), String> {
    let id = id_argument("write", arguments)?;
    let path = arguments
        .get(1)
        .filter(|path| !path.starts_with('-'))
        .ok_or("expected a container-relative path after the container id")?
        .clone();
    let flags = Flags::parse(&arguments[2..])?;
    let text = match (flags.get("text"), flags.get("file")) {
        (Some(text), _) => text.to_owned(),
        (None, Some(file)) => std::fs::read_to_string(file)
            .map_err(|error| format!("cannot read {file}: {error}"))?,
        (None, None) => return Err("expected --text <yaml> or --file <path>".to_owned()),
    };
    let section: Vec<String> = flags
        .get("section")
        .map(|section| section.split('.').filter(|key| !key.is_empty()).map(str::to_owned).collect())
        .unwrap_or_default();
    let conflict = if flags.has("overwrite") {
        "overwrite"
    } else if flags.has("refuse") {
        "refuse"
    } else {
        "merge"
    };
    let client = rpc::connect()?;
    let request = json!({
        "id": id,
        "path": path,
        "section": section,
        "text": text,
        "conflict": conflict,
        "restart": flags.has("restart"),
    });
    let value = rpc::call(&client, "enqueue_resource_write", request)?;
    let task: TaskRecord = serde_json::from_value(value)
        .map_err(|error| format!("invalid task record from daemon: {error}"))?;
    rpc::wait_task(&client, &task.id)?;
    println!(
        "wrote {}{}",
        path,
        if section.is_empty() { String::new() } else { format!("({})", section.join(".")) }
    );
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
