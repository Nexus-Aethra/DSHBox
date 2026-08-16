//! `dshbox pull <subcommand>` — fetch resources from the daemon.
//!
//! The `template` subcommand replaces the old `dshbox dsh install/ls/...`
//! commands: every installed DSH version is just a template that points at
//! the official DeepSeek harness repository.

use serde_json::json;

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(subcommand) = arguments.first().map(String::as_str) else {
        return Err("expected `dshbox pull template <ref>`".to_owned());
    };
    match subcommand {
        "template" => pull_template(arguments.get(1).ok_or(
            "expected `dshbox pull template <ref>` (e.g. `github.com/deepseek-ai/deepseek-harness:latest`)",
        )?),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown pull subcommand: {other}")),
    }
}

/// `dshbox pull template <ref>` — clone a template by reference.
///
/// `<ref>` follows the shape `github.com/<owner>/<repo>[:tag|@ref]`. The
/// `:tag` suffix is optional and defaults to `latest` when absent.
fn pull_template(ref_value: &str) -> Result<(), String> {
    let ref_value = ref_value.trim();
    if ref_value.is_empty() {
        return Err("template reference cannot be empty".to_owned());
    }
    println!("pulling template {ref_value} (this may take a while)...");
    let client = rpc::connect()?;
    rpc::run_task(
        &client,
        "pull_template",
        json!({ "ref": ref_value }),
    )?;
    println!("pulled template {ref_value}");
    Ok(())
}

fn print_help() {
    println!("dshbox pull template <ref>");
    println!();
    println!("Fetch a template by reference and materialise its base .dsh");
    println!("file under <runtime>/templates/<version>.dsh.");
    println!();
    println!("<ref> is `github.com/<owner>/<repo>[:tag|@ref]`. The :tag");
    println!("suffix is optional; a missing tag defaults to `latest`.");
    println!();
    println!("Examples:");
    println!("  dshbox pull template github.com/deepseek-ai/deepseek-harness");
    println!("  dshbox pull template github.com/deepseek-ai/deepseek-harness:latest");
    println!("  dshbox pull template github.com/deepseek-ai/deepseek-harness@v0.1.0");
}