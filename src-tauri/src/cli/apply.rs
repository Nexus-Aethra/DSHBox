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

use serde_json::{json, Value};

use super::rpc;

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

    let client = rpc::connect()?;
    // The document's meaning lives in the daemon, which owns the storage layout
    // and the order its entries depend on. This used to sequence five calls here,
    // which meant a second caller had to learn that sequence too -- and would
    // drift from it. One implementation, two front ends.
    let value = rpc::call(
        &client,
        "apply_document",
        json!({ "file": file, "dryRun": dry_run }),
    )?;
    let outcomes: Vec<Outcome> = value["outcomes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|entry| Outcome {
            action: match entry["action"].as_str().unwrap_or("") {
                "type" => "type",
                _ => "copy",
            },
            target: entry["target"].as_str().unwrap_or_default().to_owned(),
            status: match entry["status"].as_str().unwrap_or("") {
                "applied" => "applied",
                "would-apply" => "would-apply",
                "exists" => "exists",
                _ => "failed",
            },
            detail: entry["detail"]
                .as_str()
                .filter(|detail| !detail.is_empty())
                .map(str::to_owned),
        })
        .collect();

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
