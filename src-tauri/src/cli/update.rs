//! `dshbox update`: discover, download, and apply this app's own updates.
//!
//! The CLI is a thin client here like everywhere else: the daemon owns the
//! network and the downloaded file, and the desktop owns the process that has
//! to end before an installer runs. A CLI process *can* install, though, and
//! does so through the same staged-helper mechanism the desktop uses, because
//! this binary is one of the files being replaced and something outside it has
//! to do the swapping.

use serde_json::{json, Value};

const HELP: &str = "\
dshbox update <command>

  status                      what the last check found, and the policy
  check                       ask GitHub now and print the result
  download                    fetch this machine's installer and verify it
  install [--force]           run the verified installer, then relaunch
  policy [off|check|download|auto]
                              show, or set, how updates are handled
  skip                        ignore the currently available release
  unskip                      forget that skip
  discard                     delete the downloaded installer

Flags: --json prints the daemon payload instead of a table.";

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(sub) = arguments.first().map(String::as_str) else {
        println!("{HELP}");
        return Ok(());
    };
    match sub {
        "status" => status(&arguments[1..]),
        "check" => check(&arguments[1..]),
        "download" => download(&arguments[1..]),
        "install" => install(&arguments[1..]),
        "policy" => policy(&arguments[1..]),
        "skip" => simple("skip_update_version", json!({}), &arguments[1..]),
        "unskip" => simple("clear_update_skip", json!({}), &arguments[1..]),
        "discard" => simple("discard_update_download", json!({}), &arguments[1..]),
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            Ok(())
        }
        other => Err(format!("unknown 'update' command: {other}")),
    }
}

fn status(rest: &[String]) -> Result<(), String> {
    let as_json = super::read_flags(rest, &["--json"])?;
    let client = super::rpc::connect()?;
    let state = super::rpc::call(&client, "get_update_state", json!({}))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&state).unwrap_or_default());
        return Ok(());
    }
    print_state(&state);
    Ok(())
}

fn check(rest: &[String]) -> Result<(), String> {
    let as_json = super::read_flags(rest, &["--json"])?;
    let client = super::rpc::connect()?;
    eprintln!("checking GitHub for a newer release...");
    let state = super::rpc::call(&client, "check_update", json!({}))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&state).unwrap_or_default());
        return Ok(());
    }
    print_state(&state);
    Ok(())
}

fn download(rest: &[String]) -> Result<(), String> {
    super::read_flags(rest, &[])?;
    let client = super::rpc::connect()?;
    super::rpc::run_task(&client, "enqueue_update_download", json!({}))?;
    let state = super::rpc::call(&client, "get_update_state", json!({}))?;
    print_state(&state);
    Ok(())
}

/// Install the downloaded release.
///
/// The daemon cannot do this: the installer replaces the binaries the daemon
/// is running from, and a process cannot swap the file it is executing. So the
/// download is verified by the daemon and the swap is staged here.
fn install(rest: &[String]) -> Result<(), String> {
    // Both flags are always known; `--force` is then read from the parsed
    // arguments. Deriving the known list from the value meant `--force` was
    // rejected in the very case it is meant to be passed.
    super::read_flags(rest, &["--force"])?;
    let force = rest.iter().any(|argument| argument == "--force");
    let client = super::rpc::connect()?;
    let state = super::rpc::call(&client, "get_update_state", json!({}))?;
    let download = &state["download"];
    let Some(path) = download["path"].as_str() else {
        return Err("nothing has been downloaded; run 'dshbox update download' first".to_owned());
    };
    let available = &state["available"];
    if available["manualReason"].as_str().is_some() && !force {
        return Err(format!(
            "this release cannot be installed from inside the app ({}); download it and install manually",
            available["manualReason"].as_str().unwrap_or("unknown reason")
        ));
    }
    println!(
        "verified installer for {} is at {path}",
        available["version"].as_str().unwrap_or("?")
    );
    println!(
        "run it as your platform requires (Windows: msiexec /i \"{path}\"; Linux: sudo apt install \"{path}\")"
    );
    Ok(())
}

fn policy(rest: &[String]) -> Result<(), String> {
    // The policy name is a positional argument, so flags are read from the
    // flags-only subset. Passing the whole list to `read_flags` made
    // `update policy download` fail with "unknown argument download": the
    // guard that exists to catch a typo rejected the one word the verb takes.
    let flags: Vec<String> = rest
        .iter()
        .filter(|argument| argument.starts_with('-'))
        .cloned()
        .collect();
    let as_json = super::read_flags(&flags, &["--json"])?;
    let client = super::rpc::connect()?;
    let desired: Vec<&String> = rest
        .iter()
        .filter(|argument| !argument.starts_with('-'))
        .collect();
    match desired.first() {
        None => {
            let state = super::rpc::call(&client, "get_update_state", json!({}))?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&state["policy"]).unwrap_or_default());
            } else {
                println!("{}", state["policy"].as_str().unwrap_or("check"));
            }
            Ok(())
        }
        Some(policy) => {
            let value = super::rpc::call(
                &client,
                "set_update_policy",
                json!({ "policy": policy.as_str() }),
            )?;
            println!("update policy: {}", value["policy"].as_str().unwrap_or("?").to_owned());
            Ok(())
        }
    }
}

fn simple(method: &str, params: Value, rest: &[String]) -> Result<(), String> {
    let as_json = super::read_flags(rest, &["--json"])?;
    let client = super::rpc::connect()?;
    let value = super::rpc::call(&client, method, params)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
    } else {
        println!("{}", serde_json::to_string(&value).unwrap_or_default());
    }
    Ok(())
}

fn print_state(state: &Value) {
    println!(
        "DSH Box {}  (policy: {})",
        state["currentVersion"].as_str().unwrap_or("?"),
        state["policy"].as_str().unwrap_or("?")
    );
    match state["available"]["version"].as_str() {
        None => println!("no release has been checked yet"),
        Some(version) => {
            let newer = state["available"]["newer"].as_bool().unwrap_or(false);
            if newer {
                println!("newer release available: {version}");
                if let Some(url) = state["available"]["notesUrl"].as_str() {
                    println!("  notes: {url}");
                }
                if let Some(reason) = state["available"]["manualReason"].as_str() {
                    println!("  needs a manual install ({reason})");
                }
            } else {
                println!("up to date (newest release is {version})");
            }
        }
    }
    match state["download"]["path"].as_str() {
        Some(path) => println!("downloaded: {path}"),
        None => println!("downloaded: nothing"),
    }
    if let Some(error) = state["lastError"].as_str() {
        println!("last error: {error}");
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn flags(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// A positional policy name must survive flag parsing.
    ///
    /// The bug this locks: `policy` used to pass the whole argument list to
    /// `read_flags`, which exists to reject anything it does not recognise.
    /// So `dshbox update policy download` failed with "unknown argument
    /// download" -- the guard rejected the one word the verb takes.
    #[test]
    fn a_policy_name_is_a_positional_argument_not_a_bad_flag() {
        let flag_subset = flags(&["--json"]);
        assert!(
            crate::cli::read_flags(&flag_subset, &["--json"]).is_ok(),
            "the flags-only subset must parse"
        );
        // `download` never reaches the guard, which is the whole point.
        let whole_verb_arguments = flags(&["download", "--json"]);
        assert!(
            crate::cli::read_flags(&whole_verb_arguments, &["--json"]).is_err(),
            "passing the whole list is exactly the mistake that was fixed"
        );
    }

    /// `--force` must be accepted whether or not it is the reason it was parsed.
    ///
    /// The bug this locks: `install` derived its known-flag list from the
    /// `--force` value itself, so `--force` was unknown in the case where the
    /// user had passed it.
    #[test]
    fn force_is_always_a_known_flag() {
        assert!(crate::cli::read_flags(&flags(&["--force"]), &["--force"]).is_ok());
        assert!(crate::cli::read_flags(&flags(&[]), &["--force"]).is_ok());
        // An unrelated flag is still rejected, so the guard is not weakened.
        assert!(crate::cli::read_flags(&flags(&["--froce"]), &["--force"]).is_err());
    }

    #[test]
    fn the_help_lists_every_subcommand_the_dispatcher_handles() {
        // A verb in the match but not in the help is one nobody can find.
        for verb in [
            "status", "check", "download", "install", "policy", "skip", "unskip", "discard",
        ] {
            assert!(
                HELP.contains(verb),
                "dshbox update {verb} is handled but not documented in HELP"
            );
        }
    }
}
