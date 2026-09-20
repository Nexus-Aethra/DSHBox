//! `dshbox apply` — declare the Box resource layer in a document, then make it
//! so.
//!
//! The document names resource *types* (the tabs the Box UI shows, which is what
//! `+ 资源类型` adds) and *copies* (state extracted out of a container into the
//! store). One file is easier to write, review and repeat than a sequence of
//! verbs — for an agent especially, since a re-run has to be safe.
//!
//! ```yaml
//! container: test                 # default for entries that do not name one
//! types:
//!   - label: ACME state
//!     kind: path
//!     path: profile/plugins/acme/state
//! copies:
//!   - name: before-upgrade
//!     kind: credentials
//!     from: test
//! ```
//!
//! Applying is idempotent: a type is keyed by (kind, container, path) and an
//! existing copy keeps its name, so a second run reports "exists" instead of
//! adding a second copy. `--dry-run` says what would happen and changes nothing.

use serde::Deserialize;
use serde_json::{json, Value};

use super::rpc;
use box_client::RpcClient;

const HELP: &str = "\
dshbox apply -f <file.yaml|json> [--json] [--dry-run]
        make the resource layer match a document:
          container: <name>       default container for the entries below
          types:                  resource types to register (UI: + 资源类型)
            - label: <label>
              kind: <kind>        sessions | credentials | path | a plugin kind id
              path: <path>        required for kind: path
              container: <name>   overrides the document default
          copies:                 state to extract into the store
            - name: <copy name>
              kind: <kind>
              from: <container>   container the state comes out of
              path: <path>        explicit path for kind: path
              entry: <entry>      one entry only (e.g. a single session)
              out: <file.tar.gz>  also pack the payload
        --dry-run               validate and report, change nothing
        --json                  report the result as JSON

Re-applying is safe: a type is keyed by (kind, container, path), and a copy whose
name is already in the store is reported as `exists` rather than copied again.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    types: Vec<TypeSpec>,
    #[serde(default)]
    copies: Vec<CopySpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeSpec {
    kind: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    container: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CopySpec {
    name: String,
    kind: String,
    #[serde(default)]
    from: Option<String>,
    /// An explicit path, for `kind: path` — state nothing declared.
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    out: Option<String>,
}

/// What one entry did, which is also what `--json` prints.
struct Outcome {
    action: &'static str,
    target: String,
    status: &'static str,
    detail: Option<String>,
}

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let mut file: Option<String> = None;
    let mut dry_run = false;
    let mut as_json = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        match argument {
            "--file" | "-f" => {
                index += 1;
                file = Some(
                    arguments
                        .get(index)
                        .ok_or("--file needs a path")?
                        .clone(),
                );
            }
            "--dry-run" => dry_run = true,
            "--json" => as_json = true,
            "help" | "--help" | "-h" => {
                println!("{HELP}");
                return Ok(());
            }
            other if other.starts_with("--file=") => {
                file = Some(other["--file=".len()..].to_owned());
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
        index += 1;
    }
    let file = file.ok_or("expected --file <file.yaml|json>")?;
    let body = std::fs::read_to_string(&file)
        .map_err(|error| format!("cannot read {file}: {error}"))?;
    let manifest: Manifest = serde_yaml::from_str(&body)
        .map_err(|error| format!("{file} is not a valid apply document: {error}"))?;
    if manifest.types.is_empty() && manifest.copies.is_empty() {
        return Err(format!("{file} declares neither types nor copies"));
    }

    let client = rpc::connect()?;
    let containers = containers_by_name(&client)?;
    let mut outcomes: Vec<Outcome> = Vec::new();

    for (index, spec) in manifest.types.iter().enumerate() {
        let container = spec
            .container
            .clone()
            .or_else(|| manifest.container.clone())
            .ok_or_else(|| format!("types[{index}] has no container, and the document names none"))?;
        let id = resolve_container(&containers, &container)?;
        let label = spec
            .label
            .clone()
            .unwrap_or_else(|| spec.path.clone().unwrap_or_else(|| spec.kind.clone()));
        let target = format!("type {label} ({})", spec.kind);
        if dry_run {
            outcomes.push(Outcome {
                action: "type",
                target,
                status: "would-apply",
                detail: Some(format!("container {container}")),
            });
            continue;
        }
        let mut request = json!({ "kind": spec.kind, "container": id });
        if let Some(path) = &spec.path {
            request["path"] = json!(path);
        }
        if let Some(label) = &spec.label {
            request["label"] = json!(label);
        }
        match rpc::call(&client, "add_resource_view", request) {
            Ok(_) => outcomes.push(Outcome {
                action: "type",
                target,
                status: "applied",
                detail: Some(format!("container {container}")),
            }),
            Err(error) => outcomes.push(Outcome {
                action: "type",
                target,
                status: "failed",
                detail: Some(error),
            }),
        }
    }

    let existing = stored_copy_names(&client).unwrap_or_default();
    for (index, spec) in manifest.copies.iter().enumerate() {
        let container = spec
            .from
            .clone()
            .or_else(|| spec.container.clone())
            .or_else(|| manifest.container.clone())
            .ok_or_else(|| format!("copies[{index}] has no container, and the document names none"))?;
        let id = resolve_container(&containers, &container)?;
        let target = format!("copy {} ({} from {})", spec.name, spec.kind, container);
        if existing.contains(&spec.name) {
            outcomes.push(Outcome {
                action: "copy",
                target,
                status: "exists",
                detail: Some("a copy with this name is already in the store".to_owned()),
            });
            continue;
        }
        if dry_run {
            outcomes.push(Outcome {
                action: "copy",
                target,
                status: "would-apply",
                detail: None,
            });
            continue;
        }
        let mut request = json!({ "id": id, "kind": spec.kind, "name": spec.name });
        if let Some(path) = &spec.path {
            request["dest"] = json!(path);
        }
        if let Some(entry) = &spec.entry {
            request["entry"] = json!(entry);
        }
        if let Some(out) = &spec.out {
            request["out"] = json!(rpc::absolutize_path(out));
        }
        match rpc::enqueue(&client, "enqueue_resource_extract", request) {
            Ok(task) => match rpc::wait_task(&client, &task.id) {
                Ok(()) => outcomes.push(Outcome {
                    action: "copy",
                    target,
                    status: "applied",
                    detail: Some(task.id),
                }),
                Err(error) => outcomes.push(Outcome {
                    action: "copy",
                    target,
                    status: "failed",
                    detail: Some(error),
                }),
            },
            Err(error) => outcomes.push(Outcome {
                action: "copy",
                target,
                status: "failed",
                detail: Some(error),
            }),
        }
    }

    report(&outcomes, as_json)?;
    let failed = outcomes.iter().filter(|entry| entry.status == "failed").count();
    if failed > 0 {
        return Err(format!("{failed} entr{} failed", if failed == 1 { "y" } else { "ies" }));
    }
    Ok(())
}

fn report(outcomes: &[Outcome], as_json: bool) -> Result<(), String> {
    if as_json {
        let rows: Vec<Value> = outcomes
            .iter()
            .map(|entry| {
                json!({
                    "action": entry.action,
                    "target": entry.target,
                    "status": entry.status,
                    "detail": entry.detail,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    for entry in outcomes {
        println!(
            "{:<10} {:<8} {}{}",
            entry.action,
            entry.status,
            entry.target,
            entry
                .detail
                .as_ref()
                .map(|detail| format!("  ({detail})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Container id by name, so a document can say `test` instead of a uuid.
fn containers_by_name(client: &RpcClient) -> Result<Vec<(String, String)>, String> {
    let value = rpc::call(client, "list_containers", json!({}))?;
    let containers: Vec<box_containers::DshContainer> =
        serde_json::from_value(value).map_err(|error| format!("invalid container list: {error}"))?;
    Ok(containers
        .into_iter()
        .map(|container| (container.name, container.id))
        .collect())
}

fn resolve_container(containers: &[(String, String)], name: &str) -> Result<String, String> {
    if let Some((_, id)) = containers.iter().find(|(candidate, _)| candidate == name) {
        return Ok(id.clone());
    }
    // An id is accepted too: a document generated from a listing uses one.
    if containers.iter().any(|(_, id)| id == name) {
        return Ok(name.to_owned());
    }
    Err(format!(
        "no such container: {name} (have: {})",
        containers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<&str>>()
            .join(", ")
    ))
}

/// The names copies carry, not their ids: a document names a copy the way the
/// user does, and the id is `<kind>-<name>`.
fn stored_copy_names(client: &RpcClient) -> Result<Vec<String>, String> {
    let value = rpc::call(client, "list_resources", json!({}))?;
    Ok(value["resources"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}
