//! `dshbox config` — inspect and update local configuration.
//! `show` reads the config file; `set` forwards the change to the daemon so
//! both processes stay consistent.

use box_foundation::read_config;
use serde_json::json;

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected config show or config set <key> <value>".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox config show");
        println!("dshbox config set runtime <dir>");
        println!("dshbox config set mirror.github <url>");
        println!("dshbox config set mirror.npm <url>");
        return Ok(());
    }
    match action {
        "show" => show_config(),
        "set" => set_value(&arguments[1..]),
        _ => Err(format!("unknown config action: {action}")),
    }
}

fn show_config() -> Result<(), String> {
    let config = read_config()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn set_value(arguments: &[String]) -> Result<(), String> {
    let key = arguments
        .first()
        .ok_or("expected a config key: runtime, mirror.github, mirror.npm")?;
    let value = arguments
        .get(1)
        .ok_or("expected a value (use an empty string to clear)")?;
    let client = rpc::connect()?;
    match key.as_str() {
        "runtime" => {
            rpc::call(
                &client,
                "save_runtime_directory",
                json!({ "runtimeDirectory": value.clone() }),
            )?;
        }
        "mirror.github" | "mirror.npm" => {
            let config = read_config()?;
            let github = if key == "mirror.github" {
                non_empty(value)
            } else {
                config.github_mirror.clone()
            };
            let npm = if key == "mirror.npm" {
                non_empty(value)
            } else {
                config.npm_registry.clone()
            };
            rpc::call(
                &client,
                "save_mirror_settings",
                json!({ "githubMirror": github, "npmRegistry": npm }),
            )?;
        }
        _ => return Err(format!("unknown config key: {key}")),
    }
    println!("config {key} = {value}");
    Ok(())
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}
