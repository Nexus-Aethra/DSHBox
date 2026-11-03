//! Vendoring of the bundled Cordis plugins into the runtime directory.
//!
//! Shared by both binaries that used to carry private copies — the daemon
//! (startup) and the desktop shell (first launch) — whose diverging
//! clean-up semantics produced real bugs: the desktop's version removed
//! the whole `<runtime>/plugins` directory, which in an installed layout
//! deleted the `<install>/plugins/<target>` source it was about to copy.
//! This implementation cleans only the two entries it writes, is
//! digest-idempotent, and copies with `cp -rL` semantics.

use box_foundation::{copy_tree::copy_tree_following, read_config, write_config};
use std::hash::{Hash, Hasher};
use std::path::Path;

/// The platform directory name Tauri's resource map ships bundled plugins
/// under (`plugins/<target>/`).
pub fn bundled_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("windows", "x86_64") => "win-x64",
        ("windows", "aarch64") => "win-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("macos", "aarch64") => "macos-arm64",
        _ => "unsupported",
    }
}

/// Vendor `<resource>/plugins/<target>/` into `<runtime>/plugins/`.
///
/// Digest-idempotent: the manifest body is hashed together with the target
/// name and compared against `BoxConfig.plugins_manifest_digest`, so repeat
/// calls are no-ops. Defers silently while no runtime directory is
/// configured or the resource ships no plugin bundle (developer builds that
/// skipped the bundler).
pub fn initialize_bundled_plugins(resource_directory: &Path) -> Result<(), String> {
    let resource_plugins = resource_directory.join("plugins").join(bundled_target());
    let manifest_path = resource_plugins.join("plugins-manifest.json");
    if !manifest_path.is_file() {
        // Developer build that skipped the plugin bundler; the container
        // start path tolerates the missing tree.
        eprintln!(
            "bundled plugins skipped: {} not found",
            manifest_path.display()
        );
        return Ok(());
    }
    let manifest_bytes = std::fs::read(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    manifest_bytes.hash(&mut hasher);
    bundled_target().hash(&mut hasher);
    let digest = format!("{:016x}", hasher.finish());

    let config = read_config()?;
    let Some(runtime_directory) = config.runtime_directory.as_ref() else {
        return Ok(());
    };
    let runtime_root = Path::new(runtime_directory);
    if !runtime_root.is_dir() {
        return Ok(());
    }

    let cache_root = runtime_root.join("plugins");
    // The digest only proves "this manifest was vendored once"; it travels
    // inside the config and therefore survives a storage-root switch
    // (`config set runtime` / onboarding). Short-circuit only when the
    // vendored tree itself exists in the CURRENT runtime directory —
    // otherwise every container start dies with ERR_MODULE_NOT_FOUND for
    // @nexus-aethra/dsh-box-sandbox while the caller believes it is done.
    let digest_matches = config.plugins_manifest_digest.as_deref() == Some(digest.as_str());
    if digest_matches && cache_root.join("plugins-manifest.json").is_file() {
        return Ok(());
    }
    // Clean ONLY the two entries this function writes.
    //
    // In a developer checkout the resource directory is `src-tauri/resources`,
    // so the source (`.../resources/plugins/<target>`) and the destination
    // (`<runtime>/plugins`) are unrelated trees. In an INSTALLED layout the
    // resource directory is `<install>` and the source is
    // `<install>/plugins/<target>` -- a child of the destination. A
    // `remove_dir_all` on the destination therefore deleted the source it was
    // about to copy, the copy then found nothing, and the plugin was never
    // vendored anywhere. That is why every container once carried
    // `Cannot find package '@deepseek-ai/dsh-box-context'`.
    for entry in ["node_modules", "plugins-manifest.json"] {
        let path = cache_root.join(entry);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .map_err(|error| format!("cannot clean {}: {error}", path.display()))?;
        } else if path.is_file() {
            std::fs::remove_file(&path)
                .map_err(|error| format!("cannot clean {}: {error}", path.display()))?;
        }
    }
    // The copy itself is always required: the container start path reads
    // `<runtime>/plugins/node_modules/...`, while the resource ships the
    // same files one level deeper at `<runtime>/plugins/<target>/`. The only
    // thing that must not happen is deleting the source -- which is why the
    // cleanup above touches two entries instead of the whole directory.
    // Copying a source that sits *inside* the destination is fine here
    // because the per-entry cleanup never removes `<target>/`.
    copy_tree_following(&resource_plugins, &cache_root)
        .map_err(|error| format!("cannot vendor bundled plugins: {error}"))?;

    let mut updated = config;
    updated.plugins_manifest_digest = Some(digest);
    write_config(&updated)?;
    eprintln!(
        "vendored bundled plugins into {}",
        cache_root.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// DSHBOX_CONFIG_DIR is process-global; keep the env-mutating test
    /// exclusive against anything else that might read it later.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fixture(source_root: &Path, target: &str) {
        let dir = source_root.join("plugins").join(target);
        fs::create_dir_all(
            dir.join("node_modules").join("@nexus-aethra").join("dsh-box-sandbox"),
        )
        .unwrap();
        fs::write(
            dir.join("plugins-manifest.json"),
            format!(
                "{{\"target\":\"{target}\",\"pluginPackage\":\"@nexus-aethra/dsh-box-sandbox\",\"pluginVersion\":\"0.1.0\",\"builtAt\":\"2026-08-15\"}}",
            ),
        )
        .unwrap();
        fs::write(
            dir.join("node_modules")
                .join("@nexus-aethra")
                .join("dsh-box-sandbox")
                .join("index.js"),
            "export default {}\n",
        )
        .unwrap();
    }

    #[test]
    fn vendors_once_persists_the_digest_and_survives_a_repeat_call() {
        let _env = ENV_LOCK.lock().unwrap();
        let temp = std::env::temp_dir().join(format!(
            "dshbox-bundled-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let resource = temp.join("resource");
        let runtime = temp.join("runtime");
        let config_dir = temp.join("config");
        let target = bundled_target();
        fixture(&resource, target);
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&config_dir).unwrap();

        let previous = std::env::var("DSHBOX_CONFIG_DIR").ok();
        std::env::set_var("DSHBOX_CONFIG_DIR", &config_dir);
        let restore = |previous: &Option<String>| {
            match previous {
                Some(value) => std::env::set_var("DSHBOX_CONFIG_DIR", value),
                None => std::env::remove_var("DSHBOX_CONFIG_DIR"),
            }
        };

        // The config must name the runtime directory before the first call.
        let config_path = config_dir.join("config.json");
        fs::write(
            &config_path,
            format!("{{\"runtimeDirectory\":\"{}\"}}", runtime.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();

        initialize_bundled_plugins(&resource).unwrap();
        let vendored = runtime
            .join("plugins")
            .join("node_modules")
            .join("@nexus-aethra")
            .join("dsh-box-sandbox");
        assert!(vendored.join("index.js").is_file());
        assert!(runtime.join("plugins").join("plugins-manifest.json").is_file());
        // The source sits inside the destination in an installed layout and
        // must survive the per-entry cleanup.
        assert!(resource.join("plugins").join(target).is_dir());

        let config = fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("pluginsManifestDigest"), "{config}");

        // A second call with an unchanged manifest is a no-op that succeeds.
        initialize_bundled_plugins(&resource).unwrap();
        assert!(vendored.join("index.js").is_file());

        restore(&previous);
        let _ = fs::remove_dir_all(&temp);
    }
}