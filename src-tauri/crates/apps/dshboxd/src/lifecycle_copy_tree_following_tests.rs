//! copy_tree_following_tests for lifecycle.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::fs;

    fn sandbox(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-ctf-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plugin_copy_requires_a_completion_marker() {
        let source = sandbox("context-source");
        let target = sandbox("context-target");
        fs::write(source.join("package.json"), "{\"version\":1}").unwrap();
        fs::write(target.join("package.json"), "{\"version\":1}").unwrap();

        assert!(!materialized_package_matches(&source, &target).unwrap());
        fs::write(target.join(PLUGIN_COPY_MARKER), "{\"version\":1}").unwrap();
        assert!(materialized_package_matches(&source, &target).unwrap());
    }

    #[test]
    fn plugin_copy_is_complete_before_it_is_reused() {
        let source = sandbox("context-copy-source");
        let profile_modules = sandbox("context-copy-profile");
        fs::write(source.join("package.json"), "{\"version\":1}").unwrap();
        fs::create_dir_all(source.join("node_modules/example")).unwrap();
        fs::write(source.join("node_modules/example/index.js"), "export {};").unwrap();

        assert!(ensure_vendored_plugin_copied(&source, &profile_modules, SANDBOX_PLUGIN_PACKAGE).unwrap());
        let target = profile_modules.join("@nexus-aethra").join(SANDBOX_PLUGIN_ID);
        assert!(target.join(PLUGIN_COPY_MARKER).is_file());
        assert!(target.join("node_modules/example/index.js").is_file());
        assert!(!ensure_vendored_plugin_copied(&source, &profile_modules, SANDBOX_PLUGIN_PACKAGE).unwrap());
    }
