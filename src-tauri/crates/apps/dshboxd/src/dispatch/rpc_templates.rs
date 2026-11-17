//! Template RPCs: pull, build, and the container-from-template path.
//!
//! Also holds the toolchain probe and the DSH catalogue refresh, because
//! those are the two reads a template operation depends on.

use super::*;

pub(super) fn detect_toolchains() -> Result<Value, String> {
    let runtime = bundled_runtime()?;
    Ok(json!([
        { "id": "node", "name": "Node.js", "systemVersion": null, "managedVersion": runtime.node_version },
        { "id": "npm", "name": "npm", "systemVersion": null, "managedVersion": runtime.npm_version },
        { "id": "pnpm", "name": "pnpm", "systemVersion": null, "managedVersion": runtime.pnpm_version },
    ]))
}

/// Enqueue a build task on the daemon queue and return the task record
/// immediately; the client polls `task_status` for progress.
pub(super) fn enqueue_build(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let parsed: BuildImageRequest = serde_json::from_value(request.clone())
        .map_err(|error| format!("invalid build request: {error}"))?;
    let params = json!({
        "scriptPath": parsed.script_path.clone(),
        "outputPath": parsed.output_path.clone(),
        "containerName": parsed.container_name.clone(),
    });
    enqueue_task_worker(
        state,
        "image-build",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| build_sealed_template_from_script(parsed, task),
    )
    .map(HandlerResult::Async)
}


pub(super) fn enqueue_dsh_catalog_refresh(state: &DaemonState) -> Result<HandlerResult, String> {
    enqueue_task_worker(
        state,
        "dsh-catalog-refresh",
        vec!["repository:dsh-versions".to_owned()],
        json!({}),
        |_task| refresh_dsh_catalog(),
    )
    .map(HandlerResult::Async)
}

pub(super) fn enqueue_pull_template(state: &DaemonState, request: &Value) -> Result<HandlerResult, String> {
    let ref_value = request["ref"].as_str().unwrap_or("").to_owned();
    if ref_value.is_empty() {
        return Err("expected a template reference (e.g. `github.com/owner/repo[:tag|@ref]`); a missing `:tag` defaults to `latest`".to_owned());
    }
    // Resolve the version slug up front so the task is tagged with the
    // same `runtime:<version>` key the Harness tab watches for busy state.
    let version = parse_template_ref(&ref_value)
        .map_err(|error| format!("invalid template reference: {error}"))?
        .version;
    let params = json!({ "ref": ref_value.clone() });
    enqueue_task_worker(
        state,
        "template-pull",
        vec![
            format!("runtime:{version}"),
            // The shared mirror is written by every pull, so pulls serialize on
            // it rather than each racing its own clone. It stays a *third* key:
            // a pull already holds two, and the scheduler's resource set is
            // deliberately small enough to read (see the design doc).
            "harness-mirror".to_owned(),
            "repository:dsh-versions".to_owned(),
        ],
        params,
        move |task| {
            // The clone is the long pole; poll the queued task's cancel flag
            // so users can abort a slow pull from the UI.
            let cancel_manager = task.manager.clone();
            let cancel_id = task.task_id.clone();
            pull_template_with_cancel(
                ref_value,
                move || {
                    cancel_manager
                        .task(&cancel_id)
                        .map(|record| record.cancel_requested)
                        .unwrap_or(true)
                },
                task,
            )
        },
    )
    .map(HandlerResult::Async)
}


/// Materialize a template container and start its DSH host inside one
/// daemon task; the client resolves the new container's id and URL via
/// `list_containers` + `container_url` after the task settles.
pub(super) fn enqueue_template_container(
    state: &DaemonState,
    request: &Value,
) -> Result<HandlerResult, String> {
    let parsed: CreateTemplateContainerRequest = serde_json::from_value(request.clone())
        .map_err(|error| format!("invalid template container request: {error}"))?;
    let params = json!({
        "name": parsed.name.clone(),
        "template": parsed.template.clone(),
        "profile": parsed.profile.clone(),
    });
    let containers = state.containers.clone();
    enqueue_task_worker(
        state,
        "template-container",
        vec!["repository:extensions".to_owned()],
        params,
        move |task| {
            let container = create_container_from_sealed(&parsed, task)?;
            let url = start_dsh_container_inner(&container.id, &containers.running, Some(task))?;
            task.update(format!("Container {} is ready", container.id), 100);
            task.log(&format!("container url: {url}"));
            Ok(())
        },
    )
    .map(HandlerResult::Async)
}


/// Returns the resource list of a built template (the metadata-only form
/// produced by `dshbox build`), for `dshbox template show`.
pub(super) fn read_template_list(name: &str) -> Result<Value, String> {
    sealed_template_info(name)
}


/// Rich metadata for one built template: build timestamp, template version,
/// content-addressed id (manifest hash), base, profile, resource count, and
/// the parsed labels. Script templates (pulled/imported) return a slimmer
/// payload with just name, id, and imported timestamp — they have no build
/// metadata to speak of.
#[allow(dead_code, reason = "superseded by sealed_template_info RPC")]
pub(super) fn template_info(name: &str) -> Result<Value, String> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    if name.is_empty() {
        return Err("expected a template name".to_string());
    }
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;

    let index = read_template_index(&root);
    let entry = index
        .get(name)
        .ok_or_else(|| format!("template `{name}` not found"))?;

    let entry = entry.clone();

    // Script templates have no built-template manifest.
    if !entry.built {
        return Ok(json!({
            "name": entry.name,
            "id": entry.id,
            "profile": entry.profile,
            "harnessRef": entry.harness_ref,
            "built": false,
            "importedAt": entry.imported_at,
        }));
    }

    let built = read_built_template(&root, name)?
        .ok_or_else(|| format!("template `{name}` is marked built but has no manifest"))?;

    // Format ISO-ish timestamp from UNIX epoch seconds (no chrono dep).
    let created_iso = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(built.created_at))
        .map(|ts| {
            let secs = ts
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let days = secs / 86400;
            let tod = secs % 86400;
            let h = tod / 3600;
            let m = (tod % 3600) / 60;
            let s = tod % 60;

            let z: i64 = days as i64 + 719468;
            let era = if z >= 0 { z } else { z - 146096 } / 146096;
            let doe = z - era * 146096;
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y = yoe as i32 + (era * 400) as i32;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let mut month = mp as i32 + 1;
            let mut year = y as i32;
            if mp >= 10 {
                month -= 12;
                year += 1;
            }
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                year, month, d, h, m, s
            )
        })
        .unwrap_or_else(|| built.created_at.to_string());

    Ok(json!({
        "name": built.name,
        "id": entry.id,
        "built": true,
        "base": built.base,
        "profile": built.profile,
        "harnessRef": built.harness_ref,
        "schemaVersion": built.schema_version,
        "createdAt": built.created_at,
        "createdAtIso": created_iso,
        "resources": built.resources.len(),
        "labels": built.labels,
    }))
}
