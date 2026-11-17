//! tests for bundled_plugins.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// DSHBOX_CONFIG_DIR is process-global; keep the env-mutating test
    /// exclusive against anything else that might read it later.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Restores `DSHBOX_CONFIG_DIR` on drop, so an assert failure cannot leak
    /// the override into the rest of this process's test run.
    struct EnvGuard {
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var("DSHBOX_CONFIG_DIR").ok();
            std::env::set_var("DSHBOX_CONFIG_DIR", path);
            EnvGuard { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("DSHBOX_CONFIG_DIR", value),
                None => std::env::remove_var("DSHBOX_CONFIG_DIR"),
            }
        }
    }

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

        let _guard = EnvGuard::set(&config_dir);

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

        let _ = fs::remove_dir_all(&temp);
    }
