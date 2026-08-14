use super::*;
use box_extensions::transfer::{
    append_plugin_archive, archive_content_root, copy_extension_source, extract_extension_tarball,
};

pub(crate) fn is_safe_package_name(name: &str) -> bool {
    !name.is_empty() && !name.contains("..") && name.split('/').all(is_safe_identifier)
}

pub(crate) fn is_safe_workspace_relative_path(value: &str) -> bool {
    !value.is_empty() && !Path::new(value).is_absolute() && !Path::new(value).components().any(|part| matches!(part, std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_)))
}

pub(crate) fn repository_metadata(kind: &ExtensionKind, source: &Path) -> Result<(String, Option<String>, Option<String>), String> {
    match kind {
        ExtensionKind::Skill => {
            let content = fs::read_to_string(source.join("SKILL.md")).map_err(|error| error.to_string())?;
            let field = |key: &str| content.lines().find_map(|line| line.strip_prefix(key).map(str::trim)).map(|value| value.trim_matches(['\'', '"']).to_owned());
            Ok((field("name:").ok_or("skill frontmatter has no name")?, None, field("description:")))
        }
        ExtensionKind::Plugin => {
            let value: serde_json::Value = serde_json::from_str(&fs::read_to_string(source.join("package.json")).map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
            Ok((value["name"].as_str().ok_or("plugin package.json has no name")?.to_owned(), value["version"].as_str().map(str::to_owned), value["description"].as_str().map(str::to_owned)))
        }
    }
}

pub(crate) fn export_extension_directory(source: &Path, destination: &Path, task: &TaskContext) -> Result<(), String> {
    task.update("Packaging extension tarball", 30);
    box_extensions::transfer::export_extension_directory(source, destination)?;
    task.check_cancelled()?;
    task.update("Extension tarball exported", 95);
    Ok(())
}

#[tauri::command]
pub(crate) fn enqueue_container_extension_add(
    request: AddContainerExtensionRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id) || !is_safe_identifier(&request.profile) {
        return Err("invalid container or profile name".to_owned());
    }
    let source = request.source.trim().to_owned();
    if source.is_empty() {
        return Err("extension source is required".to_owned());
    }
    let task = queue_task(
        &manager,
        &app,
        "container-extension-add",
        vec![format!("container:{}", request.id)],
        serde_json::json!({ "id": request.id, "profile": request.profile, "source": source }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        install_container_extension(request, &task)
    });
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_repository_extension_import(
    request: ImportRepositoryExtensionRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if request.source.trim().is_empty() { return Err("extension source is required".to_owned()); }
    let task = queue_task(&manager, &app, "repository-extension-import", vec!["repository:extensions".to_owned()], serde_json::json!({ "source": request.source }))?;
    let task_manager = (*manager).clone(); let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| import_repository_extension(request, &task));
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_repository_extension_export(
    request: ExportRepositoryExtensionRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.repository_id) || request.destination.trim().is_empty() { return Err("invalid extension export request".to_owned()); }
    let task = queue_task(&manager, &app, "repository-extension-export", vec!["repository:extensions".to_owned()], serde_json::json!({ "repositoryId": request.repository_id, "destination": request.destination }))?;
    let task_manager = (*manager).clone(); let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| export_repository_extension(request, &task));
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_plugin_export(
    request: ExportContainerPluginRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.source_container_id)
        || request.source_path.trim().is_empty()
        || request.destination.trim().is_empty()
    {
        return Err("invalid plugin export request".to_owned());
    }
    let task = queue_task(
        &manager,
        &app,
        "plugin-export",
        vec![format!("container:{}", request.source_container_id)],
        serde_json::json!({ "sourceContainerId": request.source_container_id, "sourcePath": request.source_path, "destination": request.destination }),
    )?;
    let task_manager = (*manager).clone();
    let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| {
        export_repository_plugin(request, &task)
    });
    Ok(task)
}

#[tauri::command]
pub(crate) fn scan_container_workspace_extensions(id: String) -> Result<Vec<box_extensions::WorkspaceExtension>, String> {
    if !is_safe_identifier(&id) { return Err("invalid container id".to_owned()); }
    let root = read_config()?.runtime_directory.ok_or("DSH Box storage is not configured")?;
    let container = scan_containers(&root)?.remove(&id).ok_or("container not found")?;
    Ok(scan_workspace_extensions(&PathBuf::from(container.directory).join("workspace")))
}

#[tauri::command]
pub(crate) fn enqueue_workspace_extension_import(
    request: ImportWorkspaceExtensionRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id) || !is_safe_workspace_relative_path(&request.relative_path) { return Err("invalid workspace extension path".to_owned()); }
    let task = queue_task(&manager, &app, "workspace-extension-import", vec![format!("container:{}", request.id), "repository:extensions".to_owned()], serde_json::json!({ "id": request.id, "relativePath": request.relative_path }))?;
    let task_manager = (*manager).clone(); let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| import_workspace_extension(request, &task));
    Ok(task)
}

#[tauri::command]
pub(crate) fn enqueue_container_extension_copy(
    request: CopyRepositoryExtensionRequest,
    manager: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<TaskRecord, String> {
    if !is_safe_identifier(&request.id) || !is_safe_identifier(&request.repository_id) || request.profile.as_deref().is_some_and(|value| !is_safe_identifier(value)) { return Err("invalid extension copy request".to_owned()); }
    let task = queue_task(&manager, &app, "container-extension-copy", vec!["repository:extensions".to_owned(), format!("container:{}", request.id)], serde_json::json!({ "id": request.id, "profile": request.profile, "repositoryId": request.repository_id }))?;
    let task_manager = (*manager).clone(); let task_id = task.id.clone();
    run_queued_task(task_manager, app, task_id, move |task| copy_repository_extension(request, &task));
    Ok(task)
}

#[tauri::command]
pub(crate) fn remove_repository_extension(id: String, tasks: tauri::State<TaskManager>, app: tauri::AppHandle) -> Result<(), String> {
    if !is_safe_identifier(&id) { return Err("invalid repository extension id".to_owned()); }
    ensure_resource_idle(&tasks, "repository:extensions")?;
    let root = read_config()?.runtime_directory.ok_or("DSH Box storage is not configured")?;
    let mut entries = scan_repository(Path::new(&root));
    let entry = entries.iter().find(|entry| entry.id == id).cloned().ok_or("repository extension not found")?;
    fs::remove_dir_all(PathBuf::from(&entry.source_path).parent().ok_or("repository source has no parent")?).map_err(|error| error.to_string())?;
    entries.retain(|entry| entry.id != id);
    write_repository_index(Path::new(&root), &entries)?;
    // Dropping an extension also drops every bundle entry that referenced it,
    // so bundles never keep dangling references.
    let mut bundles = read_bundles(Path::new(&root));
    for bundle in &mut bundles {
        bundle.entries.retain(|entry| entry.repository_id != id);
    }
    write_bundles(Path::new(&root), &bundles)?;
    refresh_global_state(&app);
    Ok(())
}

#[tauri::command]
pub(crate) fn remove_repository_plugin(
    id: String,
    profile: String,
    name: String,
    tasks: tauri::State<TaskManager>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    if !is_safe_identifier(&id) || !is_safe_identifier(&profile) || !is_safe_package_name(&name) {
        return Err("invalid plugin removal request".to_owned());
    }
    ensure_resource_idle(&tasks, &format!("container:{id}"))?;
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let container = scan_containers(&root)?
        .remove(&id)
        .ok_or("container not found")?;
    let profile_directory = PathBuf::from(&container.directory)
        .join("profile/profiles")
        .join(&profile);
    let manifest_path = profile_directory.join("package.json");
    let mut manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let bundles = manifest
        .pointer_mut("/dsh/profile/bundles")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("profile has no plugin bundle list")?;
    let original = bundles.len();
    bundles.retain(|item| item.as_str() != Some(name.as_str()));
    if bundles.len() == original {
        return Err("plugin is not enabled in this profile".to_owned());
    }
    if let Some(dependencies) = manifest
        .get_mut("dependencies")
        .and_then(serde_json::Value::as_object_mut)
    {
        dependencies.remove(&name);
    }
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let link = profile_directory.join("node_modules").join(&name);
    if link.exists() {
        fs::remove_dir_all(&link).map_err(|error| error.to_string())?;
    }
    remove_plugin_record(&container, &profile, &name)?;
    refresh_global_state(&app);
    Ok(())
}

pub(crate) fn export_repository_plugin(
    request: ExportContainerPluginRequest,
    task: &TaskContext,
) -> Result<(), String> {
    let root = read_config()?
        .runtime_directory
        .ok_or("DSH Box storage is not configured")?;
    let instance_root = PathBuf::from(&root)
        .join("instances")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let source = PathBuf::from(&request.source_path)
        .canonicalize()
        .map_err(|error| format!("cannot find plugin source: {error}"))?;
    if !source.starts_with(&instance_root) || !source.join("package.json").is_file() {
        return Err("plugin source is not a DSH Box managed plugin".to_owned());
    }
    let destination = PathBuf::from(&request.destination);
    if destination.extension().and_then(|value| value.to_str()) != Some("gz") {
        return Err("plugin export destination must end in .tar.gz".to_owned());
    }
    fs::create_dir_all(
        destination
            .parent()
            .ok_or("plugin export has no parent directory")?,
    )
    .map_err(|error| error.to_string())?;
    task.update("Packaging plugin tarball", 30);
    task.log(&format!(
        "exporting {} to {}",
        source.display(),
        destination.display()
    ));
    let output = fs::File::create(&destination)
        .map_err(|error| format!("cannot create plugin tarball: {error}"))?;
    let encoder = GzEncoder::new(output, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    append_plugin_archive(&mut archive, &source, &source, Path::new("extension"))?;
    archive.finish().map_err(|error| error.to_string())?;
    task.check_cancelled()?;
    task.update("Plugin tarball exported", 95);
    Ok(())
}

pub(crate) fn import_repository_extension(request: ImportRepositoryExtensionRequest, task: &TaskContext) -> Result<(), String> {
    let root = read_config()?.runtime_directory.ok_or("DSH Box storage is not configured")?;
    let staging = repository_root(Path::new(&root)).join("staging").join(&task.task_id);
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    task.update("Importing repository source", 15);
    let source = request.source.trim();
    let extracted = if source.starts_with("https://github.com/") {
        let destination = staging.join("source");
        let config = read_config()?;
        let target = mirror_url(source, config.github_mirror.as_deref());
        task.log(&format!("cloning GitHub repository {source}"));
        let cancelled = task.clone(); shallow_clone_with_cancel(&target, &destination, None, move || cancelled.cancelled())?; destination
    } else if Path::new(source).is_dir() {
        let destination = staging.join("source"); copy_extension_source(Path::new(source), &destination)?; destination
    } else {
        let archive = PathBuf::from(source); if !archive.is_file() { return Err("tarball source must be an existing local file".to_owned()); }
        let destination = staging.join("source"); fs::create_dir_all(&destination).map_err(|error| error.to_string())?; extract_extension_tarball(&archive, &destination)?; archive_content_root(&destination)?
    };
    task.check_cancelled()?; task.update("Validating extension", 45);
    let kind = detect_extension_kind(&extracted)?;
    let (name, version, description) = repository_metadata(&kind, &extracted)?;
    let destination = repository_root(Path::new(&root)).join(match kind { ExtensionKind::Plugin => "plugins", ExtensionKind::Skill => "skills" }).join(&task.task_id).join("source");
    if destination.exists() { return Err("repository entry already exists".to_owned()); }
    fs::create_dir_all(destination.parent().ok_or("repository destination has no parent")?).map_err(|error| error.to_string())?;
    fs::rename(&extracted, &destination).map_err(|error| format!("cannot store repository source: {error}"))?;
    let mut entries = scan_repository(Path::new(&root));
    let digest = extension_digest(&destination)?;
    entries.push(RepositoryExtension { id: task.task_id.clone(), kind, name, version, description, content_digest: digest, source_path: destination.to_string_lossy().into_owned(), imported_at: now_seconds(), diagnostic: None, source: Some(source.to_owned()) });
    write_repository_index(Path::new(&root), &entries)?;
    let _ = fs::remove_dir_all(staging);
    task.update("Repository extension imported", 95); Ok(())
}

pub(crate) fn import_workspace_extension(request: ImportWorkspaceExtensionRequest, task: &TaskContext) -> Result<(), String> {
    let root = read_config()?.runtime_directory.ok_or("DSH Box storage is not configured")?;
    let container = scan_containers(&root)?.remove(&request.id).ok_or("container not found")?;
    let workspace = PathBuf::from(container.directory).join("workspace").canonicalize().map_err(|error| format!("cannot access container workspace: {error}"))?;
    let source = workspace.join(&request.relative_path).canonicalize().map_err(|error| format!("workspace extension no longer exists: {error}"))?;
    if !source.starts_with(&workspace) || !source.is_dir() { return Err("workspace extension escaped the container workspace".to_owned()); }
    task.log(&format!("copying workspace extension {}", source.display()));
    import_repository_extension(ImportRepositoryExtensionRequest { source: source.to_string_lossy().into_owned() }, task)
}

pub(crate) fn copy_repository_extension(request: CopyRepositoryExtensionRequest, task: &TaskContext) -> Result<(), String> {
    let root = read_config()?.runtime_directory.ok_or("DSH Box storage is not configured")?;
    let entry = scan_repository(Path::new(&root)).into_iter().find(|entry| entry.id == request.repository_id).ok_or("repository extension not found")?;
    if entry.diagnostic.is_some() { return Err("repository extension is invalid".to_owned()); }
    let container = scan_containers(&root)?.remove(&request.id).ok_or("container not found")?;
    task.update("Copying repository extension", 25);
    let staging = PathBuf::from(&container.directory).join("extensions/staging").join(&task.task_id).join("source");
    fs::create_dir_all(staging.parent().ok_or("extension staging has no parent")?).map_err(|error| error.to_string())?;
    copy_extension_source(Path::new(&entry.source_path), &staging)?;
    match entry.kind {
        ExtensionKind::Plugin => {
            let profile = request.profile.ok_or("plugin installation requires a profile")?;
            if !PathBuf::from(&container.directory).join("profile/profiles").join(&profile).join("package.json").is_file() { return Err(format!("profile not found: {profile}")); }
            install_container_plugin(&container, &profile, "repository", &entry.id, staging, task)?;
        }
        ExtensionKind::Skill => install_container_skill(&container, "repository", &entry.id, staging, task)?,
    }
    task.update("Container extension installed", 95); Ok(())
}

pub(crate) fn export_repository_extension(request: ExportRepositoryExtensionRequest, task: &TaskContext) -> Result<(), String> {
    let root = read_config()?.runtime_directory.ok_or("DSH Box storage is not configured")?;
    let entry = scan_repository(Path::new(&root)).into_iter().find(|entry| entry.id == request.repository_id).ok_or("repository extension not found")?;
    export_extension_directory(Path::new(&entry.source_path), Path::new(&request.destination), task)
}
