//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::path::Path;

    fn temporary_root(label: &str) -> PathBuf {
        let root = env::temp_dir().join(format!("dshbox-foundation-{label}-{}", now_nanos()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn schema_10_initialization_creates_only_new_layout() {
        let root = temporary_root("schema");
        let layout = RuntimeLayout::new(&root);
        layout.initialize_schema_10().unwrap();
        let stored: StorageSchema =
            serde_json::from_str(&fs::read_to_string(layout.storage_path()).unwrap()).unwrap();
        assert_eq!(stored.schema_version, STORAGE_SCHEMA_VERSION);
        assert!(layout.staging_dir().is_dir());
        assert!(layout.plugin_artifacts_dir().is_dir());
        assert!(layout.templates_dir().is_dir());
        assert!(!root.join("runtimes").exists());
        // A second daemon start reads and rewrites the same schema file. On
        // Windows this specifically exercises MOVEFILE_REPLACE_EXISTING.
        layout.initialize_schema_10().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_10_rejects_legacy_runtime_without_mutating_it() {
        let root = temporary_root("legacy");
        let legacy = root.join("runtimes").join("latest").join("source");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("keep.txt"), "preserve me").unwrap();
        let error = RuntimeLayout::new(&root)
            .initialize_schema_10()
            .unwrap_err();
        assert!(error.contains("unsupported pre-schema-10"));
        assert_eq!(
            fs::read_to_string(legacy.join("keep.txt")).unwrap(),
            "preserve me"
        );
        assert!(!root.join("state/storage.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staged_tree_is_published_once_without_overwrite() {
        let root = temporary_root("publish");
        let layout = RuntimeLayout::new(&root);
        layout.initialize_schema_10().unwrap();
        let staged = layout.create_staging_dir("template_build").unwrap();
        fs::write(staged.join("manifest.json"), "complete").unwrap();
        let destination = layout.sealed_template_dir("abc123").unwrap();
        layout.publish_staged_tree(&staged, &destination).unwrap();
        assert!(!staged.exists());
        assert_eq!(
            fs::read_to_string(destination.join("manifest.json")).unwrap(),
            "complete"
        );
        let retry = layout.create_staging_dir("template_build").unwrap();
        assert!(layout.publish_staged_tree(&retry, &destination).is_err());
        assert!(retry.is_dir(), "failed publish keeps retry staging intact");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sealed_manifest_requires_base_and_is_written_atomically() {
        let root = temporary_root("manifest");
        let layout = RuntimeLayout::new(&root);
        layout.initialize_schema_10().unwrap();
        let staged = layout.create_staging_dir("template_build").unwrap();
        let invalid = TemplateManifest {
            schema_version: STORAGE_SCHEMA_VERSION,
            kind: PublishedResourceKind::SealedTemplate,
            id: "sealed-1".to_owned(),
            source_ref: "github.com/deepseek-ai/deepseek-harness:latest".to_owned(),
            source_commit: "commit".to_owned(),
            node_version: "24".to_owned(),
            pnpm_version: "10".to_owned(),
            base_id: None,
            plugin_artifact_ids: Vec::new(),
            plugin_sources: Vec::new(),
            env: BTreeMap::from([("TZ".to_owned(), "Asia/Shanghai".to_owned())]),
            harness_digest: "digest".to_owned(),
            profile_digest: Some("profile".to_owned()),
            size_bytes: 1,
            validated_at: now_seconds(),
        };
        assert!(write_template_manifest(&staged, &invalid).is_err());
        let valid = TemplateManifest {
            base_id: Some("base-1".to_owned()),
            ..invalid
        };
        write_template_manifest(&staged, &valid).unwrap();
        let reread: TemplateManifest =
            serde_json::from_str(&fs::read_to_string(staged.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(reread, valid);
        // Declared ENV survives the round-trip.
        assert_eq!(
            reread.env.get("TZ").map(String::as_str),
            Some("Asia/Shanghai")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn drive_root_normalizes_to_absolute() {
        // `C:` must become a rooted path, never a drive-relative one that
        // later joins (`C:containers`) resolve against each child's drive.
        #[cfg(windows)]
        {
            let drive = env::temp_dir()
                .to_string_lossy()
                .chars()
                .next()
                .expect("temp dir starts with a drive letter")
                .to_string();
            let input = format!("{drive}:");
            let result = normalize_runtime_directory(&input).unwrap();
            let path = PathBuf::from(&result);
            assert!(path.is_absolute(), "must be absolute: {result}");
            assert!(
                !result.ends_with(':'),
                "must not stay drive-relative: {result}"
            );
        }
        #[cfg(not(windows))]
        {
            let result = normalize_runtime_directory(".").unwrap();
            assert!(PathBuf::from(&result).is_absolute());
        }
    }

    #[test]
    fn existing_absolute_path_is_canonicalized() {
        let temp = env::temp_dir().join("dsh-box-normalize-preserve");
        fs::create_dir_all(&temp).unwrap();
        let input = temp.to_string_lossy().into_owned();
        let result = normalize_runtime_directory(&input).unwrap();
        let expected = fs::canonicalize(&temp).unwrap();
        #[cfg(windows)]
        let expected = {
            // The stored value intentionally drops the verbatim prefix that
            // `std::fs::canonicalize` adds on Windows.
            PathBuf::from(without_verbatim(&expected))
        };
        assert_eq!(
            PathBuf::from(&result),
            expected,
            "stored path must match the canonical path"
        );
        fs::remove_dir_all(&temp).ok();
    }

    #[cfg(windows)]
    fn without_verbatim(path: &Path) -> String {
        let text = path.to_string_lossy();
        text.strip_prefix("\\\\?\\UNC\\")
            .map(|rest| format!("\\\\{rest}"))
            .or_else(|| text.strip_prefix("\\\\?\\").map(str::to_owned))
            .unwrap_or_else(|| text.into_owned())
    }

    #[test]
    fn nonexistent_directory_is_rejected() {
        let missing = env::temp_dir().join("dsh-box-normalize-missing-xyz");
        assert!(normalize_runtime_directory(&missing.to_string_lossy()).is_err());
    }

    #[test]
    fn mirror_url_passthrough_when_unset() {
        assert_eq!(
            mirror_url("https://api.github.com/repos/x/y/tags", None),
            "https://api.github.com/repos/x/y/tags"
        );
        assert_eq!(
            mirror_url("https://api.github.com/repos/x/y/tags", Some("")),
            "https://api.github.com/repos/x/y/tags"
        );
        assert_eq!(
            mirror_url("https://api.github.com/repos/x/y/tags", Some("   ")),
            "https://api.github.com/repos/x/y/tags"
        );
    }

    #[test]
    fn mirror_url_passthrough_for_official_github_hosts() {
        // The user pointed the mirror at GitHub itself; the original URLs
        // already use these hosts, so a prefix would duplicate the
        // authority and produce a 404.
        let url = "https://api.github.com/repos/deepseek-ai/deepseek-harness/tags?per_page=100";
        for mirror in [
            "https://api.github.com",
            "https://github.com",
            "http://github.com",
            "api.github.com",
            "  https://github.com/  ",
        ] {
            assert_eq!(mirror_url(url, Some(mirror)), url, "mirror: {mirror}");
        }
    }

    #[test]
    fn mirror_url_prefix_for_proxy() {
        // Third-party accelerators expect `<proxy>/<original-url>`.
        assert_eq!(
            mirror_url(
                "https://api.github.com/repos/x/y/tags?per_page=100",
                Some("https://gh-proxy.com")
            ),
            "https://gh-proxy.com/https://api.github.com/repos/x/y/tags?per_page=100"
        );
        // Trailing slashes are stripped so the result is well-formed.
        assert_eq!(
            mirror_url(
                "https://github.com/owner/repo.git",
                Some("https://gh-proxy.com/")
            ),
            "https://gh-proxy.com/https://github.com/owner/repo.git"
        );
        // Bare hostnames (no scheme) are still treated as a proxy.
        assert_eq!(
            mirror_url("https://github.com/owner/repo.git", Some("gh-proxy.com")),
            "gh-proxy.com/https://github.com/owner/repo.git"
        );
    }
