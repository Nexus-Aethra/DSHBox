//! `dshbox template` — list/show/import/export/rm against `<runtime>/templates`.
//!
//! Mirrors the docker-style `plugin`/`bundle` shape: every action is a thin
//! RPC against the daemon (the daemon owns the storage). The archive format
//! is a gzip tarball containing a single `<name>.dsh` file.

use serde_json::json;

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected template ls|show|import|export|rm".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox template ls");
        println!("dshbox template show <name>");
        println!("dshbox template import <archive.tar.gz> [--name <name>]");
        println!("dshbox template export <name> [<dest.tar.gz>]");
        println!("dshbox template rm <name>");
        return Ok(());
    }
    match action {
        "ls" | "list" => list(),
        "show" | "cat" => show(arguments.get(1).ok_or("expected a template name")?),
        "import" => import(arguments.get(1).ok_or("expected an archive path")?, &arguments[2..]),
        "export" => export(
            arguments.get(1).ok_or("expected a template name")?,
            arguments.get(2).map(String::as_str),
        ),
        "rm" | "remove" => remove(arguments.get(1).ok_or("expected a template name")?),
        other => Err(format!("unknown template action: {other}")),
    }
}

fn list() -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_templates", json!({}))?;
    // Shared box-api type: the CLI and the desktop passthrough deserialize
    // the very struct the daemon serializes, so neither can drift.
    let templates: Vec<box_api::TemplateInfo> = serde_json::from_value(value)
        .map_err(|error| format!("invalid template list from daemon: {error}"))?;
    println!("NAME\tVERSION\tPROFILE");
    for template in templates {
        let version = template.harness_ref.as_deref().unwrap_or("-");
        println!("{}\t{version}\t{}", template.name, template.profile);
    }
    Ok(())
}

fn show(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    let value = rpc::call(&client, "read_template", json!({ "name": name }))?;
    let text = value["text"]
        .as_str()
        .ok_or_else(|| "invalid read_template response from daemon".to_owned())?;
    print!("{text}");
    Ok(())
}

fn import(archive: &str, rest: &[String]) -> Result<(), String> {
    let name = flag_value(rest, "--name");
    let client = rpc::connect()?;
    let mut params = json!({ "archive": rpc::absolutize_path(archive) });
    if let Some(name) = name {
        params["name"] = json!(name);
    }
    let value = rpc::call(&client, "import_template", params)?;
    let imported = value["name"].as_str().unwrap_or("(unknown)");
    println!("imported template `{imported}` from {archive}");
    Ok(())
}

fn export(name: &str, destination: Option<&str>) -> Result<(), String> {
    let client = rpc::connect()?;
    let mut params = json!({ "name": name });
    if let Some(destination) = destination {
        params["destination"] = json!(rpc::absolutize_path(destination));
    }
    let value = rpc::call(&client, "export_template", params)?;
    let path = value["path"]
        .as_str()
        .ok_or_else(|| "invalid export_template response from daemon".to_owned())?;
    println!("exported template `{name}` to {path}");
    Ok(())
}

fn remove(name: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::call(&client, "remove_template", json!({ "name": name }))?;
    println!("removed template `{name}`");
    Ok(())
}

fn flag_value(arguments: &[String], flag: &str) -> Option<String> {
    arguments
        .windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .filter(|value| !value.is_empty())
}