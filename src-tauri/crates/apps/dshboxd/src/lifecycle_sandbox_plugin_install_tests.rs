//! sandbox_plugin_install_tests for lifecycle.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dshbox-install-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_scoped_plugin_lands_under_its_own_scope() {
        let source = scratch("scope-src");
        let profile_modules = scratch("scope-profile");
        fs::write(source.join("package.json"), "{\"version\":1}").unwrap();

        assert!(ensure_vendored_plugin_copied(&source, &profile_modules, SANDBOX_PLUGIN_PACKAGE).unwrap());

        let (scope, id) = SANDBOX_PLUGIN_PACKAGE.split_once('/').unwrap();
        let target = profile_modules.join(scope).join(id);
        assert!(target.is_dir(), "plugin must exist at {}", target.display());
        assert!(target.join("package.json").is_file());
        assert!(!profile_modules.join("@deepseek-ai").join("dsh-box-sandbox").exists());
    }

    #[test]
    fn the_skill_lands_in_the_container_skill_root() {
        let container = scratch("skill-container");
        let bundled = scratch("skill-bundled");
        fs::write(bundled.join("SKILL.md"), "v1").unwrap();

        install_box_skill(&container, &bundled, None).unwrap();
        let installed = container.join("profile/skills").join("dsh-box-tools").join("SKILL.md");
        assert!(installed.is_file(), "skill must be at {}", installed.display());
        assert_eq!(fs::read_to_string(&installed).unwrap(), "v1");

        fs::write(bundled.join("SKILL.md"), "v2").unwrap();
        install_box_skill(&container, &bundled, None).unwrap();
        assert_eq!(fs::read_to_string(&installed).unwrap(), "v2");
    }

    #[test]
    fn a_missing_bundled_skill_is_skipped_not_fatal() {
        let container = scratch("skill-missing");
        let bundled = scratch("skill-missing-src");
        install_box_skill(&container, &bundled, None).unwrap();
        assert!(!container.join("profile/skills/dsh-box-tools/SKILL.md").exists());
    }
