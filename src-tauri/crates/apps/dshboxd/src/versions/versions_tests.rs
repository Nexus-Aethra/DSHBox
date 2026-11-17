//! tests for versions.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    #[test]
    fn prepared_harness_validation_requires_the_dependency_cache_and_the_client_artifacts() {
        let temporary = tempfile::tempdir().unwrap();
        let harness = temporary.path().join("harness");
        fs::create_dir_all(harness.join("node_modules/tsx")).unwrap();
        fs::create_dir_all(harness.join("apps/cli/src")).unwrap();
        fs::write(harness.join("package.json"), "{}").unwrap();
        fs::write(harness.join("apps/cli/src/bin.ts"), "").unwrap();
        assert!(validate_prepared_harness(&harness).is_err());
        fs::write(harness.join("node_modules/tsx/package.json"), "{}").unwrap();
        // Windows-only optional-package check: the fixture needs the esbuild
        // and koffi virtual-store entries before validation can pass.
        for package in [
            "@esbuild+win32-x64@0.21.5",
            "@koromix+koffi-win32-x64@3.1.1",
        ] {
            fs::create_dir_all(
                harness
                    .join("node_modules/.pnpm")
                    .join(package)
                    .join("node_modules"),
            )
            .unwrap();
        }
        // The client build runs while the base is prepared, so a base without
        // its artifacts is incomplete.
        assert!(validate_prepared_harness(&harness).is_err());
        fs::create_dir_all(harness.join("apps/web/dist")).unwrap();
        fs::write(harness.join("apps/web/dist/index.html"), "").unwrap();
        assert!(validate_prepared_harness(&harness).is_err());
        crate::sealed::write_client_artifact_marker(&harness, "abc").unwrap();
        // The host starts the built CLI entry, so a base whose build stopped
        // before producing it cannot serve a container.
        assert!(validate_prepared_harness(&harness).is_err());
        fs::create_dir_all(harness.join("apps/cli/lib")).unwrap();
        fs::write(harness.join("apps/cli/lib/bin.js"), "").unwrap();
        validate_prepared_harness(&harness).unwrap();
    }

    #[test]
    fn client_artifacts_are_only_trusted_for_the_commit_and_platform_they_were_built_for() {
        let temporary = tempfile::tempdir().unwrap();
        let harness = temporary.path().join("harness");
        fs::create_dir_all(harness.join("apps/web/dist")).unwrap();
        fs::write(harness.join("apps/web/dist/index.html"), "").unwrap();
        fs::create_dir_all(harness.join("apps/cli/lib")).unwrap();
        fs::write(harness.join("apps/cli/lib/bin.js"), "").unwrap();
        // No marker: an old template, or one that lost it. Build, do not trust.
        assert!(!crate::sealed::client_artifacts_present(&harness, "abc"));
        crate::sealed::write_client_artifact_marker(&harness, "abc").unwrap();
        assert!(crate::sealed::client_artifacts_present(&harness, "abc"));
        // A marker that claims artifacts the tree does not carry — the entry
        // the host starts is missing — must not be trusted either.
        fs::remove_file(harness.join("apps/cli/lib/bin.js")).unwrap();
        assert!(!crate::sealed::client_artifacts_present(&harness, "abc"));
        fs::write(harness.join("apps/cli/lib/bin.js"), "").unwrap();
        // A template imported from another machine carries that machine's
        // native addon, so its artifacts are not this platform's.
        assert!(!crate::sealed::client_artifacts_present(&harness, "def"));
        fs::write(
            harness.join(crate::sealed::CLIENT_ARTIFACT_MARKER),
            r#"{"commit":"abc","platform":"plan9-sparc"}"#,
        )
        .unwrap();
        assert!(!crate::sealed::client_artifacts_present(&harness, "abc"));
    }

    #[test]
    fn prepared_base_index_round_trips_without_runtime_paths_in_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        let layout = RuntimeLayout::new(temporary.path());
        layout.initialize_schema_10().unwrap();
        let mut index = super::prepared_base_index::PreparedBaseIndex::new();
        index.insert(
            "latest".to_owned(),
            PreparedBaseRecord {
                id: "base-abc".to_owned(),
                version: "latest".to_owned(),
                source_ref: "github.com/deepseek-ai/deepseek-harness:latest".to_owned(),
                source_commit: "abc".to_owned(),
                directory: layout
                    .prepared_base_dir("abc")
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                created_at: 1,
                size_bytes: 2,
            },
        );
        write_prepared_base_index(&layout, &index).unwrap();
        assert_eq!(
            read_prepared_base_index(&layout)
                .unwrap()
                .get("latest")
                .unwrap()
                .id,
            "base-abc"
        );
    }
