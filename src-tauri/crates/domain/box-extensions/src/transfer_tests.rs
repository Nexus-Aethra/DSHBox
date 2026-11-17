//! tests for transfer.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    /// A fresh sandbox root per call. The suffix is a UUID, not
    /// `now_seconds()`: a wall-clock second is coarse enough that two tests in
    /// the same binary -- or a re-run inside the same second -- used to land on
    /// the SAME path, so a directory left behind by an earlier run was adopted
    /// as this test's starting state. The UUID makes the directory unique per
    /// call, so no run can read or delete another's rows; each test still
    /// removes its own at the end.
    fn sandbox(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dshbox-transfer-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Inverted: with the cp -rL install path the on-disk files must be
    /// *distinct*. Asserting this is cheap on Unix (compare inodes) and
    /// remains useful on Windows because two physical copies will at
    /// minimum have different canonical paths — but a Windows test would
    /// need a more careful file-id comparison, so we skip it there.
    #[cfg(unix)]
    fn assert_distinct_copies(left: &Path, right: &Path) {
        use std::os::unix::fs::MetadataExt;
        let left_ino = fs::metadata(left).unwrap().ino();
        let right_ino = fs::metadata(right).unwrap().ino();
        assert_ne!(left_ino, right_ino, "{left:?} and {right:?} must NOT share an inode");
    }

    #[cfg(not(unix))]
    fn assert_distinct_copies(left: &Path, right: &Path) {
        let left_canon = fs::canonicalize(left).unwrap();
        let right_canon = fs::canonicalize(right).unwrap();
        assert_ne!(left_canon, right_canon, "{left:?} and {right:?} must NOT share a path");
    }

    #[test]
    fn locate_root_returns_directory_itself_when_it_holds_manifest() {
        let root = sandbox("root-itself");
        fs::write(root.join("package.json"), "{}").unwrap();
        assert_eq!(locate_extension_root(&root).unwrap(), root);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locate_root_descends_two_levels_and_prefers_shallowest() {
        let root = sandbox("locate-two");
        fs::create_dir_all(root.join("packages/deep/skill")).unwrap();
        fs::write(root.join("packages/deep/skill/SKILL.md"), "# s").unwrap();
        fs::create_dir_all(root.join("vendor/vendored")).unwrap();
        fs::write(root.join("vendor/vendored/package.json"), "{}").unwrap();
        // Depth 1 beats depth 2 when both hold a manifest.
        fs::create_dir_all(root.join("shallow")).unwrap();
        fs::write(root.join("shallow/package.json"), "{}").unwrap();
        let found = locate_extension_root(&root).unwrap();
        assert_eq!(found, root.join("shallow"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locate_root_rejects_beyond_two_levels() {
        let root = sandbox("locate-limit");
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::write(root.join("a/b/c/package.json"), "{}").unwrap();
        assert!(locate_extension_root(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn locate_root_skips_noise_directories() {
        let root = sandbox("locate-noise");
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/package.json"), "{}").unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist/package.json"), "{}").unwrap();
        // Only noise matches below the root: nothing real to find.
        assert!(locate_extension_root(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn copy_skips_git_and_node_modules() {
        let root = sandbox("copy");
        let source = root.join("source");
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("package.json"), "{}").unwrap();
        fs::write(source.join(".git/HEAD"), "ref").unwrap();
        fs::write(source.join("node_modules/pkg/index.js"), "x").unwrap();
        fs::write(source.join("src/index.ts"), "export").unwrap();
        let target = root.join("target");
        copy_extension_source(&source, &target).unwrap();
        assert!(target.join("package.json").is_file());
        assert!(target.join("src/index.ts").is_file());
        assert!(!target.join(".git").exists());
        assert!(!target.join("node_modules").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn export_and_extract_round_trip_preserves_content() {
        let root = sandbox("roundtrip");
        let plugin = root.join("plugin");
        fs::create_dir_all(plugin.join(".git")).unwrap();
        fs::create_dir_all(plugin.join("lib")).unwrap();
        fs::write(plugin.join("package.json"), r#"{"name":"demo","version":"1.0.0"}"#)
            .unwrap();
        fs::write(plugin.join("lib/main.js"), "console.log(1)").unwrap();
        fs::write(plugin.join(".git/config"), "ignored").unwrap();
        let tarball = root.join("demo.tar.gz");
        export_extension_directory(&plugin, &tarball).unwrap();
        let extracted = root.join("extracted");
        fs::create_dir_all(&extracted).unwrap();
        extract_extension_tarball(&tarball, &extracted).unwrap();
        let content_root = archive_content_root(&extracted).unwrap();
        assert!(content_root.join("package.json").is_file());
        assert!(content_root.join("lib/main.js").is_file());
        assert!(!content_root.join(".git").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_conflict_keeps_bracketed_names_and_overwrite_keeps_original() {
        let entries = vec![RepositoryExtension {
            id: "a".to_owned(),
            kind: ExtensionKind::Plugin,
            name: "demo".to_owned(),
            version: None,
            description: None,
            content_digest: "d".to_owned(),
            source_path: "p".to_owned(),
            imported_at: 0,
            diagnostic: None,
            source: None,
            storage: crate::RepositoryStorage::Owned,
            derived: false,
        }];
        assert_eq!(
            resolve_conflict_name(&entries, &ExtensionKind::Plugin, "demo", "keep"),
            "demo (2)"
        );
        assert_eq!(
            resolve_conflict_name(&entries, &ExtensionKind::Plugin, "demo", "overwrite"),
            "demo"
        );
        assert_eq!(
            resolve_conflict_name(&entries, &ExtensionKind::Skill, "demo", "keep"),
            "demo"
        );
    }
