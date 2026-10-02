//! `dshbox plugin` — the extension and skill repository, plus per-container
//! plugin installs and listings. Thin client: every action serializes an
//! RPC against the daemon and prints the response.

use box_client::RpcClient;
use serde_json::json;

use super::rpc;

pub(crate) fn command(arguments: &[String]) -> Result<(), String> {
    let Some(action) = arguments.first().map(String::as_str) else {
        return Err("expected plugin ls|import|export|rm|prune|install|refs".to_owned());
    };
    if matches!(action, "help" | "--help" | "-h") {
        println!("dshbox plugin ls [container] [--profile <name>]");
        println!("dshbox plugin import <source>   a package (name or npm:name@version) is\n                                       recorded as a pointer into the pnpm store; a local\n                                       directory is copied in");
        println!("dshbox plugin export <id> <dest.tar.gz>");
        println!("dshbox plugin rm <id>");
        println!("dshbox plugin prune");
        println!("dshbox plugin refs [--verbose]");
        println!("dshbox plugin install <container> <spec|entry-id> [--profile <name>]");
        println!("                                       <spec|entry-id>: a package spec, URL, tarball");
        println!("                                       or local path, or an id from plugin ls");
        return Ok(());
    }
    match action {
        "ls" | "list" if arguments.len() >= 2 => container_plugins(&arguments[1..]),
        "ls" | "list" => repository_list(&arguments[1..]),
        "import" => repository_import(
            arguments
                .get(1)
                .ok_or("expected a source: a package name, npm:name@version, or a local directory")?,
        ),
        "export" => repository_export(
            arguments.get(1).ok_or("expected a repository entry id")?,
            arguments.get(2).ok_or("expected a destination path")?,
        ),
        "rm" => repository_remove(arguments.get(1).ok_or("expected a repository entry id")?),
        "prune" => repository_prune(&arguments[1..]),
        "refs" => repository_refs(&arguments[1..]),
        "install" | "add" => container_plugin_add(&arguments[1..]),
        _ => Err(format!("unknown plugin action: {action}")),
    }
}

fn repository_list(rest: &[String]) -> Result<(), String> {
    let as_json = super::read_flags(rest, &["--json"])?;
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_repository_extensions", json!({}))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return Ok(());
    }
    let entries: Vec<box_extensions::RepositoryExtension> = serde_json::from_value(value)
        .map_err(|error| format!("invalid repository list from daemon: {error}"))?;
    println!("ID\tKIND\tNAME\tVERSION\tSTORAGE");
    for entry in entries {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            entry.id,
            kind_name(&entry.kind),
            entry.name,
            entry.version.as_deref().unwrap_or("-"),
            match entry.storage {
                box_extensions::RepositoryStorage::Owned => "copy",
                box_extensions::RepositoryStorage::Reference => "reference",
            }
        );
    }
    Ok(())
}

fn kind_name(kind: &box_extensions::ExtensionKind) -> &'static str {
    match kind {
        box_extensions::ExtensionKind::Plugin => "plugin",
        box_extensions::ExtensionKind::Skill => "skill",
    }
}

fn repository_import(source: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::run_task(
        &client,
        "import_repository_extension",
        json!({ "source": rpc::absolutize_path(source) }),
    )?;
    println!("imported repository entry from {source}");
    Ok(())
}

fn repository_export(id: &str, destination: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::run_task(
        &client,
        "export_repository_extension",
        json!({
            "repositoryId": id,
            "destination": rpc::absolutize_path(destination),
        }),
    )?;
    println!("exported repository entry {id} to {destination}");
    Ok(())
}

fn repository_remove(id: &str) -> Result<(), String> {
    let client = rpc::connect()?;
    rpc::call(&client, "remove_repository_extension", json!({ "id": id }))?;
    println!("removed repository entry {id}");
    Ok(())
}

/// Print every repository entry alongside the container / template ids
/// that currently reference it. Useful when `plugin rm` or `plugin prune`
/// reports a "still in use" error and the user wants to know which owner
/// is blocking the delete. Pass `--verbose` to expand the id columns.
fn repository_refs(rest: &[String]) -> Result<(), String> {
    let verbose = rest.iter().any(|argument| argument == "--verbose");
    let as_json = super::read_flags(rest, &["--json", "--verbose"])?;
    let client = rpc::connect()?;
    let value = rpc::call(&client, "list_repository_reference_counts", json!({}))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return Ok(());
    }
    let rows: Vec<box_extensions::RepositoryReferenceRow> = serde_json::from_value(value)
        .map_err(|error| format!("invalid reference rows from daemon: {error}"))?;
    if rows.is_empty() {
        println!("no repository entries");
        return Ok(());
    }
    if verbose {
        println!("ID\tKIND\tNAME\tVERSION\tCONTAINERS\tTEMPLATES");
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}\t[{}]\t[{}]",
                row.id,
                kind_name(&row.kind),
                row.name,
                row.version.as_deref().unwrap_or("-"),
                row.containers.join(", "),
                row.templates.join(", "),
            );
        }
    } else {
        println!("ID\tKIND\tNAME\tVERSION\tCONTAINERS\tTEMPLATES");
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                row.id,
                kind_name(&row.kind),
                row.name,
                row.version.as_deref().unwrap_or("-"),
                row.containers.len(),
                row.templates.len(),
            );
        }
    }
    Ok(())
}

/// Deletes repository entries whose reference count dropped to zero (no
/// container links them anymore). Entries still in use are left untouched.
fn repository_prune(rest: &[String]) -> Result<(), String> {
    // Nothing to honour, but said aloud: a verb that takes no flag must not
    // silently accept one.
    super::read_flags(rest, &[])?;
    let client = rpc::connect()?;
    let value = rpc::call(&client, "prune_repository_extensions", json!({}))?;
    let removed: Vec<String> = serde_json::from_value(value)
        .map_err(|error| format!("invalid prune response from daemon: {error}"))?;
    if removed.is_empty() {
        println!("no unused repository entries to prune");
    } else {
        for id in &removed {
            println!("pruned unused repository entry {id}");
        }
    }
    Ok(())
}

fn container_plugins(arguments: &[String]) -> Result<(), String> {
    let id = arguments.first().ok_or("expected container id")?;
    let profile = selected_profile(arguments);
    let client = rpc::connect()?;
    let value = rpc::call(
        &client,
        "container_list_plugins",
        json!({ "containerId": id, "profile": profile }),
    )?;
    let plugins: Vec<String> = serde_json::from_value(value)
        .map_err(|error| format!("invalid plugin list from daemon: {error}"))?;
    for plugin in plugins {
        println!("{plugin}");
    }
    Ok(())
}

/// Does this argument name a row in Box's extension repository rather than
/// a pnpm package?
///
/// Repository ids are minted by the daemon in exactly two shapes, and neither
/// is ever a package spec:
///
/// * `img-<task uuid>` — an owned copy, built by the import task
///   (`dshboxd/src/extensions.rs`, which appends `-2`, `-3`… when one build
///   task imports several plugins)
/// * `ref-<fnv1a64 hex>` — a store-backed reference
///   (`box-extensions/src/lib.rs`)
///
/// Matching the prefixes rather than the full shape keeps the `-2` suffix and
/// any future widening working, and a false positive only costs a clear
/// "no such entry" error instead of a registry 404. **If a third id prefix is
/// ever introduced, add it here** — otherwise that shape silently falls
/// through to pnpm, which is the bug this routing exists to fix.
fn is_repository_entry_id(source: &str) -> bool {
    source.starts_with("img-") || source.starts_with("ref-")
}

/// Install into a container from either a pnpm spec or a repository entry id.
///
/// The two are routed separately because a repository id is a *row id*, not a
/// package name. `container_plugin_add` splices its `spec` verbatim into the
/// pnpm argv, so handing it `img-<uuid>` asks the public registry for a
/// package with that literal name and fails with
/// `ERR_PNPM_FETCH_404` — a 404 that mentions neither the repository nor the
/// container, which is what made this so hard to diagnose.
///
/// `enqueue_container_extension_copy` is the route that understands ids: the
/// daemon looks the row up and then branches on how the entry is stored. A
/// `Reference` entry is installed from its pnpm spec; an `Owned` entry is
/// copied straight into the profile and needs no subprocess at all. The
/// second case is the reason ids have to be routed rather than translated
/// here — an owned copy has no registry spec for pnpm to fetch at all.
fn container_plugin_add(arguments: &[String]) -> Result<(), String> {
    let id = arguments.first().ok_or("expected container id")?;
    let source = arguments.get(1).ok_or(
        "expected a package spec, URL, tarball, local path, or a repository entry id",
    )?;
    let profile = selected_profile(arguments);
    let client = rpc::connect()?;

    if is_repository_entry_id(source) {
        install_repository_entry(&client, id, source, &profile)?;
        println!("installed repository entry {source} into {id} (profile {profile})");
        return Ok(());
    }

    rpc::run_task(
        &client,
        "container_plugin_add",
        json!({ "containerId": id, "profile": profile, "spec": source }),
    )?;
    println!("installed {source} into {id} (profile {profile})");
    Ok(())
}

/// Does the repository contain a row with this id?
///
/// Checked before a task is enqueued rather than by matching the daemon error
/// text: the daemon reports a miss as a bare "repository extension not found",
/// and a task that exists only to fail is noise in the task list.
fn repository_entry_exists(client: &RpcClient, id: &str) -> Result<bool, String> {
    let value = rpc::call(client, "list_repository_extensions", json!({}))?;
    let entries: Vec<box_extensions::RepositoryExtension> = serde_json::from_value(value)
        .map_err(|error| format!("invalid repository list from daemon: {error}"))?;
    Ok(entries.iter().any(|entry| entry.id == id))
}

/// Install a repository entry by id, through the path that understands ids.
///
/// The container key here is `id`, not `containerId` as on
/// `container_plugin_add`: this handler predates that convention and reads
/// `request["id"]`, so the difference is load-bearing.
fn install_repository_entry(
    client: &RpcClient,
    container_id: &str,
    repository_id: &str,
    profile: &str,
) -> Result<(), String> {
    if !repository_entry_exists(client, repository_id)? {
        return Err(format!(
            "no repository entry with id {repository_id}; run \"dshbox plugin ls\" to list \
             the ids. To install a package whose name merely starts with the same \
             prefix, pass a full package spec such as name@version",
        ));
    }
    rpc::run_task(
        client,
        "enqueue_container_extension_copy",
        json!({ "id": container_id, "profile": profile, "repositoryId": repository_id }),
    )
}

fn selected_profile(arguments: &[String]) -> String {
    arguments
        .windows(2)
        .find(|pair| pair[0] == "--profile")
        .map(|pair| pair[1].clone())
        .unwrap_or_else(|| "web".to_owned())
}
