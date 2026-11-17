//! tests for manifest.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use crate::script::parse_script;
    use std::path::PathBuf;

    #[test]
    fn round_trip_v6_manifest() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
             PROFILE web
             NAME team-stack
             VERSION 1.2.3
             ADD plugin github.com/team/foo:1.2.3
             ADD plugin cordis-plugin-bar
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        let json = serialize_manifest(&manifest).unwrap();
        let restored = parse_manifest(&json).unwrap();
        assert_eq!(restored, manifest);
    }

    #[test]
    fn rejects_wrong_schema_version() {
        let json = r#"{
            "schemaVersion": 99,
            "mediaType": "application/vnd.dshbox.template.v6+json",
            "id": "x:1",
            "name": "x",
            "version": "1",
            "createdAt": 0,
            "base": { "type": "runtime" },
            "profile": "web",
            "adds": []
        }"#;
        assert!(parse_manifest(json).is_err());
    }

    #[test]
    fn harness_is_first_add() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        assert!(!manifest.adds.is_empty());
        let first = &manifest.adds[0];
        assert!(matches!(first.source, AddSource::Github { .. }));
    }

    #[test]
    fn user_plugins_are_adds() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
             PROFILE web
             ADD plugin cordis-plugin-foo
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        // harness + 1 plugin = 2 adds
        assert_eq!(manifest.adds.len(), 2);
        let plugin = &manifest.adds[1];
        assert_eq!(plugin.kind, AddKind::Plugin);
        assert_eq!(plugin.resource_type, ResourceType::Code);
        assert!(plugin.destination.contains("cordis-plugin-foo"));
    }

    #[test]
    fn data_adds_are_marked_as_data_resources() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\n             PROFILE web\n             ADD data team-docs\n             ADD plugin cordis-plugin-foo\n",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        // harness + 1 data + 1 plugin = 3 adds
        assert_eq!(manifest.adds.len(), 3);
        let data = &manifest.adds[1];
        assert_eq!(data.kind, AddKind::Data);
        assert_eq!(data.resource_type, ResourceType::Data);
        assert!(data.destination.starts_with("extensions/data/"));
        let plugin = &manifest.adds[2];
        assert_eq!(plugin.resource_type, ResourceType::Code);
    }

    #[test]
    fn duplicate_plugins_are_separate_adds() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
             PROFILE web
             ADD plugin cordis-plugin-foo@1.0.0
             ADD plugin cordis-plugin-foo@2.0.0
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        // harness + 2 plugins = 3 adds (duplicates are separate layers)
        assert_eq!(manifest.adds.len(), 3);
    }

    #[test]
    fn builtin_defs_are_present() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
",
            &PathBuf::from("/tmp"),
        )
        .unwrap();
        let manifest = compile_manifest(&script, 1_700_000_000);
        assert!(manifest.defs.contains_key("plugin"));
        assert!(manifest.defs.contains_key("skill"));
        assert!(manifest.defs.contains_key("data"));
    }
