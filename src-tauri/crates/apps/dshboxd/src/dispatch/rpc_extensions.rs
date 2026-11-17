//! Extension repository and plugin RPCs: list, import, export, install into a
//! container, and the dependency graph.
//!
//! The repository is independent of containers (AGENTS.md); importing copies
//! into a container, and later repository edits never mutate the installed copy.

use super::*;

pub(super) fn list_bundles() -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(json!(read_bundles(std::path::Path::new(&root))))
}

pub(super) fn list_repository_extensions() -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    Ok(json!(scan_repository(std::path::Path::new(&root))))
}

/// Debugging aid: every repository entry paired with the ids of the
/// containers and built templates currently linked to it. Reads from
/// `references.json` exactly as it sits on disk (no reconciliation) —
/// the read path is fast on purpose, and `plugin rm` / `plugin prune`
/// / `template rm` / `container rm` all run `reconcile_owner_index`
/// before they mutate.
pub(super) fn list_repository_reference_counts_rpc() -> Result<Value, String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let entries = scan_repository(std::path::Path::new(&root));
    let references = box_extensions::read_references(std::path::Path::new(&root));
    let rows: Vec<Value> = entries
        .into_iter()
        .map(|entry| {
            let owners = references.get(&entry.id);
            let containers: Vec<String> = owners.map(|_| Vec::new()).unwrap_or_default();
            let templates: Vec<String> = owners
                .map(|set| set.templates.iter().cloned().collect())
                .unwrap_or_default();
            json!({
                "id": entry.id,
                "name": entry.name,
                "kind": entry.kind,
                "version": entry.version,
                "containers": containers,
                "templates": templates,
            })
        })
        .collect();
    Ok(json!(rows))
}


/// Read the cordis service graph for one template or container.
///
/// Read-only and fast enough to answer inline: it walks the plugin sources under
/// the harness tree plus the profile's installed plugins and reads the literal
/// `inject` / service-registration declarations out of them. `kind` decides
/// whether `id` names a sealed template or a container.
/// `@scope/name@1.2.3` → name + version; anything else is a name (a `link:` or
/// `file:` spec has no registry version to read).
pub(super) fn split_spec(spec: &str) -> (String, Option<String>) {
    if let Some((name, version)) = spec.rsplit_once('@') {
        if !name.is_empty()
            && version
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
        {
            return (name.to_owned(), Some(version.to_owned()));
        }
    }
    (spec.to_owned(), None)
}


pub(super) fn plugin_dependency_graph_rpc(request: &Value) -> Result<Value, String> {
    use box_plugin_graph::{build_graph, GraphPlugin, GraphSource, Half, ScanRoots};

    let id = request["id"].as_str().unwrap_or("").to_owned();
    if id.is_empty() {
        return Err("plugin_dependency_graph requires an id".to_owned());
    }
    let kind = request["kind"].as_str().unwrap_or("template");
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;

    let (source, profile, directory) = match kind {
        "template" => {
            let (profile, directory) = template_root_by_name(&root, &id)?;
            (GraphSource::Template, profile, directory)
        }
        "container" => {
            if !box_foundation::is_safe_identifier(&id) {
                return Err("invalid container id".to_owned());
            }
            let container = box_containers::scan_containers(&root)?
                .remove(&id)
                .ok_or_else(|| format!("container not found: {id}"))?;
            (
                GraphSource::Container,
                container.profile.clone(),
                std::path::PathBuf::from(&container.directory),
            )
        }
        other => return Err(format!("unknown plugin graph source `{other}`")),
    };

    let mut graph = build_graph(
        source,
        &id,
        &profile,
        &ScanRoots {
            harness: Some(directory.join("harness")),
            profile: Some(directory.join("profile").join("profiles").join(&profile)),
            repository: Some(std::path::Path::new(&root).join("repository")),
        },
        now_seconds(),
    );
    // A sealed template's tree holds the base, not the plugins its boxfile added:
    // those live in the manifest's recipe and are installed when a container is
    // created. Scanning therefore finds none of them, so they are added here —
    // otherwise the preview of a template omits the very package its boxfile names.
    if matches!(source, GraphSource::Template) {
        let manifest = std::fs::read_to_string(directory.join("manifest.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let recipes: Vec<String> = manifest
            .as_ref()
            .and_then(|value| value.get("pluginSources"))
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        // pnpm's own resolution, from the lockfile the sealed template keeps.
        // It is the only record of what will be installed — the tree has no
        // `node_modules` — and it names the plugins a bundle pulled in that no
        // boxfile ever wrote down.
        let locked = std::fs::read_to_string(
            directory
                .join("profile")
                .join("profiles")
                .join(&profile)
                .join("pnpm-lock.yaml"),
        )
        .ok()
        .and_then(|text| box_plugin_graph::lockfile::parse_lockfile(&text).ok())
        .unwrap_or_default();

        for spec in recipes {
            if spec.is_empty() {
                continue;
            }
            let (name, spec_version) = split_spec(&spec);
            let version = locked
                .iter()
                .find(|package| package.name == name)
                .map(|package| package.version.clone())
                .or(spec_version);
            if !graph.plugins.iter().any(|plugin| plugin.name == name) {
                graph.plugins.push(GraphPlugin {
                    id: name.clone(),
                    name: name.clone(),
                    half: Half::Host,
                    version,
                    activated: true,
                    source: "template recipe — installed when a container is created".to_owned(),
                    provides: Vec::new(),
                    requires: Vec::new(),
                    inserts: Vec::new(),
                });
            }
            graph.recipe_plugins.push(name);
        }

        // Plugins the recipe pulls in without naming them: the version is the
        // one pnpm resolved, not the one a boxfile guessed.
        for package in locked.iter().filter(|package| package.plugin) {
            if graph
                .plugins
                .iter()
                .any(|plugin| plugin.name == package.name)
            {
                continue;
            }
            graph.plugins.push(GraphPlugin {
                id: package.name.clone(),
                name: package.name.clone(),
                half: Half::Host,
                version: Some(package.version.clone()),
                activated: true,
                source: "pnpm lock — a recipe dependency pulled this plugin in".to_owned(),
                provides: Vec::new(),
                requires: Vec::new(),
                inserts: Vec::new(),
            });
            graph.recipe_plugins.push(package.name.clone());
        }
        if !locked.is_empty() {
            graph
                .diagnostics
                .push(box_plugin_graph::lockfile::summary(&locked));
        }
        let transitive = locked
            .iter()
            .filter(|package| package.plugin && !package.direct)
            .count();
        if transitive > 0 {
            graph.diagnostics.push(format!(
                "{transitive} plugin(s) are not named by the boxfile: the profile's pnpm lock pulls them in, and their versions above are pnpm's resolution"
            ));
        }
        if !graph.recipe_plugins.is_empty() {
            graph.diagnostics.push(format!(
                "{} plugin(s) come from this template's recipe (the boxfile and the pnpm lock) and are installed when a container is created, so their sources are not in the template tree",
                graph.recipe_plugins.len()
            ));
        }
    }
    serde_json::to_value(graph).map_err(|error| format!("cannot serialize plugin graph: {error}"))
}


pub(super) fn enqueue_repository_import(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let source = request["source"].as_str().unwrap_or("").to_owned();
    if source.is_empty() {
        return Err("expected a source path".to_owned());
    }
    let params = json!({ "source": source.clone() });
    enqueue_task_worker(
        state,
        "repository-extension-import",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| import_source(task, &source),
    )
    .map(HandlerResult::Async)
}

pub(super) fn enqueue_repository_export(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let repository_id = request["repositoryId"].as_str().unwrap_or("").to_owned();
    let destination = request["destination"].as_str().unwrap_or("").to_owned();
    if repository_id.is_empty() || destination.is_empty() {
        return Err("expected repositoryId and destination".to_owned());
    }
    let params = json!({
        "repositoryId": repository_id.clone(),
        "destination": destination.clone(),
    });
    enqueue_task_worker(
        state,
        "repository-extension-export",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| export_repository_extension(&repository_id, &destination, task),
    )
    .map(HandlerResult::Async)
}

pub(super) fn enqueue_container_plugin_add(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let container_id = request["containerId"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("web").to_owned();
    let spec = request["spec"].as_str().unwrap_or("").to_owned();
    if container_id.is_empty() || spec.is_empty() {
        return Err("expected containerId and spec".to_owned());
    }
    // The worker resolves the container itself, from `read_config` rather than
    // from this state, so it can only be handed a canonical id.
    let container_id = crate::resolve::resolve_container_key(state, &container_id)?;
    let params = json!({
        "containerId": container_id.clone(),
        "profile": profile.clone(),
        "spec": spec.clone(),
    });
    enqueue_task_worker(
        state,
        "container-plugin-add",
        vec!["container:extensions".to_owned()],
        params,
        move |task| container_plugin_add(&container_id, &profile, &spec, task),
    )
    .map(HandlerResult::Async)
}


/// Parse a string array parameter.
pub(super) fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}


pub(super) fn uninstall_dsh_version_rpc(request: &Value) -> Result<Value, String> {
    let version = request["version"].as_str().unwrap_or("").to_owned();
    uninstall_dsh_version(&version)?;
    Ok(json!({ "version": version }))
}

pub(super) fn remove_repository_extension_rpc(request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    remove_repository_extension(&id)?;
    Ok(json!({ "id": id }))
}

pub(super) fn delete_extension_bundle_rpc(request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    delete_extension_bundle(&id)?;
    Ok(json!({ "id": id }))
}


pub(super) fn enqueue_container_extension_add(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("web").to_owned();
    let source = request["source"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id) || !box_foundation::is_safe_identifier(&profile) {
        return Err("invalid container or profile name".to_owned());
    }
    if source.trim().is_empty() {
        return Err("extension source is required".to_owned());
    }
    // `install_container_extension` looks the container up itself and takes no
    // state, so the alias has to be spent here before the closure captures it.
    let id = crate::resolve::resolve_container_key(state, &id)?;
    let params = json!({
        "id": id.clone(),
        "profile": profile.clone(),
        "source": source.clone(),
    });
    enqueue_task_worker(
        state,
        "container-extension-add",
        vec![format!("container:{id}")],
        params,
        move |task| install_container_extension(&id, &profile, &source, task),
    )
    .map(HandlerResult::Async)
}

pub(super) fn enqueue_workspace_extension_import(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let relative_path = request["relativePath"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id) || !is_safe_workspace_relative_path(&relative_path)
    {
        return Err("invalid workspace extension path".to_owned());
    }
    // Same reason as the plugin install above: the importer reads the container
    // from the runtime root, not from this state.
    let id = crate::resolve::resolve_container_key(state, &id)?;
    let params = json!({ "id": id.clone(), "relativePath": relative_path.clone() });
    enqueue_task_worker(
        state,
        "workspace-extension-import",
        vec![
            format!("container:{id}"),
            "repository:extensions".to_owned(),
        ],
        params,
        move |task| import_workspace_extension(&id, &relative_path, task),
    )
    .map(HandlerResult::Async)
}

pub(super) fn enqueue_container_extension_copy(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().map(str::to_owned);
    let repository_id = request["repositoryId"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&id)
        || !box_foundation::is_safe_identifier(&repository_id)
        || profile
            .as_deref()
            .is_some_and(|value| !box_foundation::is_safe_identifier(value))
    {
        return Err("invalid extension copy request".to_owned());
    }
    // The copy worker installs into the container by scanning for this id, so a
    // name or a bare timestamp would install nothing and report not-found for a
    // container that is right there.
    let id = crate::resolve::resolve_container_key(state, &id)?;
    let params = json!({ "id": id.clone(), "repositoryId": repository_id.clone() });
    enqueue_task_worker(
        state,
        "container-extension-copy",
        vec![
            "repository:extensions".to_owned(),
            format!("container:{id}"),
        ],
        params,
        // `container_extension_copy` is invoked from the resources page to
        // manually link a plugin into one container; no template is in
        // play, so we record the container as the owner of the
        // repository entry (None template_id triggers that branch in
        // `link_repository_extension`).
        move |task| link_repository_extension(&id, profile.as_deref(), &repository_id, None, task),
    )
    .map(HandlerResult::Async)
}


pub(super) fn enqueue_plugin_export(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let source_container_id = request["sourceContainerId"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let source_path = request["sourcePath"].as_str().unwrap_or("").to_owned();
    let destination = request["destination"].as_str().unwrap_or("").to_owned();
    if !box_foundation::is_safe_identifier(&source_container_id)
        || source_path.trim().is_empty()
        || destination.trim().is_empty()
    {
        return Err("invalid plugin export request".to_owned());
    }
    let params = json!({ "sourceContainerId": source_container_id.clone(), "sourcePath": source_path.clone(), "destination": destination.clone() });
    enqueue_task_worker(
        state,
        "plugin-export",
        vec![format!("container:{source_container_id}")],
        params,
        move |task| {
            export_repository_plugin(&source_container_id, &source_path, &destination, task)
        },
    )
    .map(HandlerResult::Async)
}


pub(super) fn remove_repository_plugin_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("").to_owned();
    let name = request["name"].as_str().unwrap_or("").to_owned();
    // This edits one profile's manifest rather than deleting a container, so it
    // resolves like any other verb; the response echoes the canonical id so a
    // caller that removes twice by name reaches the same container both times.
    let id = crate::resolve::resolve_container_key(state, &id)?;
    remove_repository_plugin(&id, &profile, &name)?;
    Ok(json!({ "id": id, "removed": true }))
}


pub(super) fn enqueue_container_bundle_install(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let id = request["id"].as_str().unwrap_or("").to_owned();
    let profile = request["profile"].as_str().unwrap_or("").to_owned();
    let bundle_id = request["bundleId"].as_str().unwrap_or("").to_owned();
    let conflict = request["conflict"].as_str().unwrap_or("keep").to_owned();
    if !box_foundation::is_safe_identifier(&id)
        || !box_foundation::is_safe_identifier(&profile)
        || !box_foundation::is_safe_identifier(&bundle_id)
        || !matches!(conflict.as_str(), "overwrite" | "keep")
    {
        return Err("invalid bundle install request".to_owned());
    }
    let params = json!({
        "id": id.clone(),
        "profile": profile.clone(),
        "bundleId": bundle_id.clone(),
        "conflict": conflict.clone(),
    });
    enqueue_task_worker(
        state,
        "container-bundle-install",
        vec![format!("container:{id}")],
        params,
        move |task| install_container_bundle(&id, &profile, &bundle_id, &conflict, task),
    )
    .map(HandlerResult::Async)
}
