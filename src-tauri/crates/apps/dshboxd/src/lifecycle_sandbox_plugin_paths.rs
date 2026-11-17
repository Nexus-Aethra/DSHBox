//! sandbox_plugin_paths for lifecycle.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dshbox-paths-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// The vendored tree must resolve as `<runtime>/plugins/node_modules/<scoped name>`
    /// -- exactly the layout `initialize_bundled_plugins` writes. Joining the whole
    /// scoped name onto a hardcoded scope produced
    /// `@deepseek-ai/@nexus-aethra/...`, which nothing ever creates, so the plugin
    /// was silently skipped on every start.
    #[test]
    fn the_vendored_tree_is_found_under_its_own_scope() {
        let runtime = scratch("vendored");
        let pkg = runtime.join("plugins/node_modules").join(SANDBOX_PLUGIN_PACKAGE);
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("package.json"), "{}").unwrap();

        let resolved = runtime
            .join("plugins")
            .join("node_modules")
            .join(SANDBOX_PLUGIN_PACKAGE);
        assert!(resolved.join("package.json").is_file(), "looked at {}", resolved.display());

        assert!(!runtime
            .join("plugins/node_modules/@deepseek-ai")
            .join(SANDBOX_PLUGIN_PACKAGE)
            .exists());
    }

    /// `vendored` is the package directory, so its skill lives at
    /// `<vendored>/skills/dsh-box-tools` -- not one level deeper.
    #[test]
    fn the_bundled_skill_sits_under_the_package_not_another_level() {
        let runtime = scratch("skillpath");
        let pkg = runtime.join("plugins/node_modules").join(SANDBOX_PLUGIN_PACKAGE);
        let skill = pkg.join("skills").join("dsh-box-tools");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "x").unwrap();

        assert!(pkg.join("skills").join("dsh-box-tools").join("SKILL.md").is_file());
        assert!(!pkg.join("skills").join("dsh-box-sandbox").exists());
    }
