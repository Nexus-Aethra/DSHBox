//! tests for resources.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use box_resources::kinds::Shape;

    fn record(id: &str) -> ResourceRecord {
        ResourceRecord {
            id: id.to_owned(),
            kind: "sessions".to_owned(),
            name: id.to_owned(),
            source_container: "container-1".to_owned(),
            source_path: "profile/sessions".to_owned(),
            digest: "d".to_owned(),
            bytes: 1,
            files: 1,
            secret: false,
            shape: Shape::Entries,
            entry_depth: 2,
            plugin: None,
            created_at: 0,
        }
    }

    /// A copy taken from one Harness layout has to land where the destination
    /// keeps that state, not where its source kept it.
    #[test]
    fn a_slotted_part_retargets_to_the_destinations_layout() {
        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("container-1");
        std::fs::create_dir_all(target.join("profile/profiles/web")).unwrap();
        let write_version = |version: &str| {
            std::fs::create_dir_all(target.join("harness")).unwrap();
            std::fs::write(
                target.join("harness/package.json"),
                format!(r#"{{"name":"@deepseek-ai/dsh","version":"{version}"}}"#),
            )
            .unwrap();
        };
        let part = |path: &str, section: &[&str]| transfer::PayloadPart {
            path: path.to_owned(),
            section: section.iter().map(|step| (*step).to_owned()).collect(),
            slot: Some("provider-route".to_owned()),
        };

        write_version("0.1.6-alpha.2");
        let parts = retarget_parts(
            &target,
            "web",
            "credentials",
            vec![
                transfer::PayloadPart {
                    path: "profile/.credentials.yaml".to_owned(),
                    section: Vec::new(),
                    slot: None,
                },
                part("profile/profiles/web/cordis.patch.yml", &["#llm-pi-ai"]),
            ],
        );
        assert_eq!(
            parts[0].path, "profile/.credentials.yaml",
            "an unslotted part keeps the path it was extracted with"
        );
        assert_eq!(parts[1].path, "profile/settings.yaml");
        assert_eq!(parts[1].section, ["llm-pi-ai"]);

        write_version("0.1.7-alpha.1");
        let parts = retarget_parts(
            &target,
            "web",
            "credentials",
            vec![part("profile/settings.yaml", &["llm-pi-ai"])],
        );
        assert_eq!(parts[0].path, "profile/profiles/web/cordis.patch.yml");
        assert_eq!(parts[0].section, ["#llm-pi-ai"]);
    }

    #[test]
    fn taking_a_copy_again_adds_one_instead_of_replacing_it() {
        let collection = Collection::memory("resources", record::record_key);
        assert_eq!(
            free_record_id(&collection, "sessions-hello").unwrap(),
            "sessions-hello"
        );
        collection.upsert(&[record("sessions-hello")]).unwrap();
        assert_eq!(
            free_record_id(&collection, "sessions-hello").unwrap(),
            "sessions-hello-2"
        );
        collection.upsert(&[record("sessions-hello-2")]).unwrap();
        assert_eq!(
            free_record_id(&collection, "sessions-hello").unwrap(),
            "sessions-hello-3"
        );
        // A different name is unaffected.
        assert_eq!(
            free_record_id(&collection, "credentials-hello").unwrap(),
            "credentials-hello"
        );
    }

    /// A view written before the `enabled` field existed was a visible tab,
    /// so reading it as disabled would hide a type the user pinned on purpose.
    #[test]
    fn a_view_without_an_enabled_flag_reads_as_enabled() {
        let stored = serde_json::json!({
            "id": "sessions-container-1",
            "label": "Chat history",
            "kind": "sessions",
            "container": "container-1",
            "path": "profile/sessions",
            "secret": false,
            "shape": "entries",
            "entryDepth": 2,
            "createdAt": 0,
        });
        let view: box_resources::ResourceView = serde_json::from_value(stored).unwrap();
        assert!(
            view.enabled,
            "an older view must not disappear from the navigation"
        );
    }

    /// Closing a type is not deleting it: the row and the copies taken under
    /// it survive, and only the navigation stops offering the tab.
    #[test]
    fn closing_a_view_keeps_the_row() {
        let collection = Collection::memory("resource_views", record::view_key);
        let view = box_resources::ResourceView {
            id: "sessions-container-1".to_owned(),
            label: "Chat history".to_owned(),
            kind: "sessions".to_owned(),
            container: "container-1".to_owned(),
            path: Some("profile/sessions".to_owned()),
            secret: false,
            shape: Shape::Entries,
            entry_depth: 2,
            enabled: true,
            created_at: 7,
        };
        collection.upsert(&[view.clone()]).unwrap();

        // The switch only changes the flag.
        let mut closed = view.clone();
        closed.enabled = false;
        collection.upsert(&[closed]).unwrap();

        let stored = collection.load_all().unwrap();
        assert_eq!(stored.len(), 1, "closing a type must not remove its row");
        assert!(!stored[0].enabled);
        assert_eq!(stored[0].label, "Chat history", "the label is the user's");
        assert_eq!(stored[0].created_at, 7, "closing does not reorder the type");
    }

    #[test]
    fn a_copy_without_a_name_is_named_after_its_container() {
        let root =
            std::env::temp_dir().join(format!("dshbox-resource-name-{}", std::process::id()));
        let container = root.join("instances").join("container-1");
        std::fs::create_dir_all(&container).unwrap();
        std::fs::write(
            container.join("container.json"),
            r#"{"id":"container-1","name":"hello"}"#,
        )
        .unwrap();
        assert_eq!(container_name(&root, "container-1"), "hello");
        // A container that is not registered yet still names the copy.
        assert_eq!(container_name(&root, "container-2"), "container-2");
        let _ = std::fs::remove_dir_all(root);
    }
