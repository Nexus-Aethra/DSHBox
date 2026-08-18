//! `dshbox image` — DEPRECATED alias of the template commands.
//!
//! DSH Box has a single construction unit: the TEMPLATE. `dshbox build`
//! produces a *built* template (metadata-only resource list) stored in the
//! same content-addressable store as pulled script templates, and
//! `dshbox run <template>` creates containers from either form. The word
//! "image" survives only as this backwards-compatible alias
//! (image ls|show|rm|prune|build → template equivalents).

use super::{build, template};

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err(
            "expected image build <script>|ls|show|rm|prune (alias of 'dshbox template')"
                .to_owned(),
        );
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("'dshbox image' is a deprecated alias — the construct is a TEMPLATE:");
        println!("  dshbox image build <script.dsh> [--output <path>] [--name <template>]  -> dshbox build");
        println!("  dshbox image ls | show <name> | rm <name> | prune                      -> dshbox template ...");
        return Ok(());
    }
    eprintln!("note: 'dshbox image' is an alias; prefer 'dshbox template' / 'dshbox build'.");
    match action {
        "build" => {
            let script_path = arguments.get(1).ok_or("expected a script path")?;
            let output_path = arguments
                .windows(2)
                .find(|pair| pair[0] == "--output")
                .map(|pair| pair[1].clone());
            let name = arguments
                .windows(2)
                .find(|pair| pair[0] == "--name")
                .map(|pair| pair[1].clone());
            build::enqueue(script_path, output_path, name)
        }
        // Everything else maps 1:1 onto the template actions.
        _ => template::command(arguments),
    }
}
