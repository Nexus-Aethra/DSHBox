//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::{fs, path::Path};

    fn container(root: &Path) -> DshContainer {
        DshContainer {
            id: "one".to_owned(),
            name: "One".to_owned(),
            version: "latest".to_owned(),
            profile: "web".to_owned(),
            template: None,
            env: Default::default(),
            runtime: None,
            directory: root.to_string_lossy().into_owned(),
            status: "stopped".to_owned(),
        }
    }

    #[test]
    fn scans_profiles_scoped_plugins_and_container_skills() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-extension-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(root.join("profile/profiles/web/node_modules/@scope/plugin")).unwrap();
        fs::create_dir_all(root.join("profile/skills/demo")).unwrap();
        fs::write(
            root.join("profile/profiles/web/package.json"),
            r#"{"dsh":{"profile":{"bundles":["@scope/plugin","missing"]}}}"#,
        )
        .unwrap();
        fs::write(
            root.join("profile/profiles/web/node_modules/@scope/plugin/package.json"),
            r#"{"name":"@scope/plugin","version":"1.2.3","description":"Plugin"}"#,
        )
        .unwrap();
        fs::write(
            root.join("profile/skills/demo/SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\n",
        )
        .unwrap();
        let found = scan_container_extensions(&container(&root));
        assert_eq!(
            found.profiles[0].plugins[0].version.as_deref(),
            Some("1.2.3")
        );
        assert!(found.profiles[0].plugins[1].diagnostic.is_some());
        assert_eq!(found.skills[0].name, "demo");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reference_owners_are_added_removed_and_idempotent() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-references-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(repository_root(&root)).unwrap();

        // Absent entries count as zero.
        assert_eq!(reference_count(&root, "img-a"), 0);
        assert!(unused_repository_ids(&root).is_empty());

        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        assert_eq!(reference_count(&root, "img-a"), 1);

        // Persistence: a fresh read sees both sets.
        let snapshot = read_references(&root);
        assert_eq!(snapshot["img-a"].templates.len(), 1);

        // Empty entries are dropped from the on-disk map so the file
        // stays compact for the unused check.
        remove_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        assert!(read_references(&root).get("img-a").is_none());

        // An entry with at least one owner survives unused_repository_ids.
        write_repository_index(
            &root,
            &[RepositoryExtension {
                id: "img-a".to_owned(),
                kind: ExtensionKind::Plugin,
                name: "a".to_owned(),
                version: None,
                description: None,
                content_digest: "d".to_owned(),
                source_path: "missing".to_owned(),
                imported_at: 0,
                diagnostic: None,
                source: None,
                storage: RepositoryStorage::Owned,
                derived: false,
            }],
        )
        .unwrap();
        assert_eq!(unused_repository_ids(&root), vec!["img-a"]);
        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-1").unwrap();
        assert!(unused_repository_ids(&root).is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_numeric_references_are_dropped_on_read() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-references-legacy-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(repository_root(&root)).unwrap();
        // Oldest format: bare numeric value. Reads as empty; a subsequent
        // write drops the entry from the file entirely.
        std::fs::write(references_path(&root), "{\"img-a\": 3}").unwrap();
        assert!(read_references(&root).is_empty());

        add_reference_owner(&root, "img-a", ReferenceKind::Template, "tpl-x").unwrap();
        let raw = std::fs::read_to_string(references_path(&root)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["img-a"]["templates"], serde_json::json!(["tpl-x"]));
        assert!(
            parsed["img-a"].get("containers").is_none()
                || parsed["img-a"]["templates"].as_array().unwrap().len() == 1
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reconcile_rebuilds_owner_map_from_canonical_sources() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-reconcile-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let home = std::env::temp_dir().join(format!(
            "dshbox-reconcile-home-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(repository_root(&root)).unwrap();
        fs::create_dir_all(home.join(".dsh-box")).unwrap();
        fs::write(
            home.join(".dsh-box/config.json"),
            serde_json::json!({ "runtimeDirectory": root.to_string_lossy() }).to_string(),
        )
        .unwrap();
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: single-threaded test, restored on drop.
        unsafe { std::env::set_var("HOME", &home) };

        // Build a container workspace with one repository-linked plugin.
        let container_root = root.join("instances/container-alpha");
        fs::create_dir_all(&container_root).unwrap();
        // `box_containers::scan_containers` only returns containers
        // that have a `container.json` sidecar; write one so the
        // reconcile pass sees `container-alpha` as a live owner.
        fs::write(
            container_root.join("container.json"),
            serde_json::json!({
                "id": "container-alpha",
                "name": "alpha",
                "version": "v1",
                "profile": "web",
                "directory": container_root.to_string_lossy(),
                "status": "stopped",
            })
            .to_string(),
        )
        .unwrap();
        let record = ExtensionRecord {
            kind: ExtensionKind::Plugin,
            name: "@scope/repo-plugin".to_owned(),
            source_kind: "repository".to_owned(),
            source: "img-real".to_owned(),
            profile: Some("web".to_owned()),
            path: "/repo/path".to_owned(),
            installed_at: now_seconds(),
            repository_id: Some("img-real".to_owned()),
            content_digest: None,
        };
        write_extension_record(
            &DshContainer {
                id: "container-alpha".to_owned(),
                name: "alpha".to_owned(),
                version: "v1".to_owned(),
                profile: "web".to_owned(),
                template: None,
                env: Default::default(),
                runtime: None,
                directory: container_root.to_string_lossy().into_owned(),
                status: "stopped".to_owned(),
            },
            record,
        )
        .unwrap();

        // Pre-seed references.json with stale entries that disagree with
        // the canonical sources.
        let mut seeded = BTreeMap::new();
        seeded.insert(
            "img-orphan".to_owned(),
            ReferenceCount {
                templates: BTreeSet::from(["tpl-orphan".to_owned()]),
                ..ReferenceCount::default()
            },
        );
        write_references(&root, &seeded).unwrap();

        let report = reconcile_owner_index(&root).unwrap();
        // `img-orphan` has no canonical reference, so the whole entry
        // (its template owner set) is wiped from the on-disk map.
        assert_eq!(report.templates_pruned, 1, "orphan entry dropped");

        let after = read_references(&root);
        assert!(after.get("img-orphan").is_none());

        if let Some(value) = prev_home {
            unsafe { std::env::set_var("HOME", value) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn detects_skill_and_dsh_plugin_roots() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-detect-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let skill = root.join("skill");
        let plugin = root.join("plugin");
        fs::create_dir_all(&skill).unwrap();
        fs::create_dir_all(&plugin).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: skill\n---\n").unwrap();
        fs::write(
            plugin.join("package.json"),
            r#"{"dsh":{"bundle":{"patch":"./cordis.patch.yml"}}}"#,
        )
        .unwrap();
        assert_eq!(detect_extension_kind(&skill).unwrap(), ExtensionKind::Skill);
        assert_eq!(
            detect_extension_kind(&plugin).unwrap(),
            ExtensionKind::Plugin
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn derived_entries_mirror_the_install_and_spare_owned_rows() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-derived-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(repository_root(&root)).unwrap();
        upsert_reference_entry(
            &root,
            ExtensionKind::Plugin,
            "hand-made",
            Some("1.0.0"),
            "npm:hand-made@1.0.0",
        )
        .unwrap();
        write_repository_index(
            &root,
            &[
                scan_repository(&root)[0].clone(),
                RepositoryExtension {
                    id: "img-owned".to_owned(),
                    kind: ExtensionKind::Plugin,
                    name: "owned".to_owned(),
                    version: Some("9.9.9".to_owned()),
                    description: None,
                    content_digest: "d".to_owned(),
                    source_path: root.join("source/owned").to_string_lossy().into_owned(),
                    imported_at: 0,
                    diagnostic: None,
                    source: None,
                    storage: RepositoryStorage::Owned,
                    derived: false,
                },
            ],
        )
        .unwrap();

        let wanted = vec![DerivedEntry {
            name: "@scope/plugin".to_owned(),
            version: "1.2.3".to_owned(),
            spec: "@scope/plugin@1.2.3".to_owned(),
        }];
        // The user's own row for `hand-made` is left as it is, and the mirror
        // does not add a second row for the same name.
        assert_eq!(
            sync_derived_entries(&root, ExtensionKind::Plugin, &wanted).unwrap(),
            1
        );

        // A second pass changes nothing: the row is keyed by its spec.
        assert_eq!(
            sync_derived_entries(&root, ExtensionKind::Plugin, &wanted).unwrap(),
            0
        );
        let entries = scan_repository(&root);
        let derived: Vec<&RepositoryExtension> =
            entries.iter().filter(|entry| entry.derived).collect();
        assert_eq!(derived.len(), 1);
        assert_eq!(derived[0].name, "@scope/plugin");
        assert_eq!(derived[0].source.as_deref(), Some("@scope/plugin@1.2.3"));
        assert_eq!(derived[0].version.as_deref(), Some("1.2.3"));

        // Nothing installs the plugin any more: the mirror drops its row. The
        // user's own row for `hand-made` stays even though pnpm resolves that
        // name too.
        let only_user = vec![DerivedEntry {
            name: "hand-made".to_owned(),
            version: "2.0.0".to_owned(),
            spec: "npm:hand-made@2.0.0".to_owned(),
        }];
        assert_eq!(
            sync_derived_entries(&root, ExtensionKind::Plugin, &only_user).unwrap(),
            0
        );
        let rows: Vec<(String, bool)> = scan_repository(&root)
            .into_iter()
            .map(|entry| (entry.name, entry.derived))
            .collect();
        assert_eq!(
            rows,
            vec![("hand-made".to_owned(), false), ("owned".to_owned(), false),]
        );
        let _ = fs::remove_dir_all(root);
    }
