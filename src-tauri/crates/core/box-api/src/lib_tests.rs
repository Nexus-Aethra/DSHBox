//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    /// Round-trip a pinned JSON fixture through the struct: the wire shape
    /// is part of the protocol between daemon, desktop, and CLI, so any
    /// field change must update the fixture deliberately.
    fn fixture_roundtrip<T>(json: &str)
    where
        T: serde::de::DeserializeOwned + Serialize + PartialEq + std::fmt::Debug,
    {
        let parsed: T = serde_json::from_str(json).expect("fixture must deserialize");
        let rendered = serde_json::to_value(&parsed).expect("must serialize");
        assert_eq!(rendered, serde_json::from_str::<serde_json::Value>(json).unwrap());
    }

    #[test]
    fn template_info_wire_shape() {
        fixture_roundtrip::<TemplateInfo>(
            r#"{"name":"github.com/deepseek-ai/deepseek-harness:latest","id":"a1b2c3d4e5f60718","harnessRef":"latest","profile":"web","built":false}"#,
        );
        // `harnessRef` may be absent for imported templates; `built`
        // defaults to false on older payloads.
        let parsed: TemplateInfo = serde_json::from_str(
            r#"{"name":"local.dsh","id":"0000000000000001","profile":"web"}"#,
        )
        .unwrap();
        assert_eq!(parsed.harness_ref, None);
        assert!(!parsed.built);
    }

    #[test]
    fn template_text_wire_shape() {
        fixture_roundtrip::<TemplateText>(r#"{"name":"base.dsh","text":"FROM base\n"}"#);
    }

    #[test]
    fn build_image_request_wire_shape() {
        fixture_roundtrip::<BuildImageRequest>(
            r#"{"scriptPath":"/tmp/boxfile.dsh","outputPath":null,"containerName":"demo"}"#,
        );
    }

    #[test]
    fn create_template_container_request_wire_shape() {
        fixture_roundtrip::<CreateTemplateContainerRequest>(
            r#"{"name":"demo","template":"github.com/deepseek-ai/deepseek-harness:latest","profile":null}"#,
        );
    }

    fn sample_resource_list() -> TemplateResourceList {
        TemplateResourceList {
            schema_version: TEMPLATE_LIST_SCHEMA_VERSION,
            name: "demo".to_owned(),
            base: "github.com/deepseek-ai/deepseek-harness:latest".to_owned(),
            profile: "web".to_owned(),
            harness_ref: Some("latest".to_owned()),
            labels: BTreeMap::new(),
            created_at: 1_786_900_000,
            resources: vec![
                TemplateResource {
                    kind: "plugin".to_owned(),
                    name: "dsh-better-sidebar".to_owned(),
                    source_kind: "plugin".to_owned(),
                    source: "repository/dsh-better-sidebar".to_owned(),
                    destination: "node_modules/dsh-better-sidebar".to_owned(),
                    sha256: "1c5b6d2c98d6e9f0a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718".to_owned(),
                },
                TemplateResource {
                    kind: "skill".to_owned(),
                    name: "boxfile-guide".to_owned(),
                    source_kind: "skill".to_owned(),
                    source: "data/feedface01234567".to_owned(),
                    destination: "profile/skills/boxfile-guide".to_owned(),
                    sha256: "feedface0123456701234567890abcdef01234567890abcdef01234567890abcde".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn template_resource_list_wire_shape() {
        // The spec (docs/specs/image-build.md) pins this JSON layout; any
        // field change must update the spec deliberately.
        let json = serde_json::to_value(sample_resource_list()).unwrap();
        assert_eq!(json["schemaVersion"], 8);
        assert_eq!(json["harnessRef"], "latest");
        let plugin = &json["resources"][0];
        assert_eq!(plugin["kind"], "plugin");
        assert_eq!(plugin["sourceKind"], "plugin");
        assert_eq!(plugin["source"], "repository/dsh-better-sidebar");
        assert_eq!(plugin["destination"], "node_modules/dsh-better-sidebar");
        let skill = &json["resources"][1];
        assert_eq!(skill["kind"], "skill");
        assert_eq!(skill["sourceKind"], "skill");
        assert_eq!(skill["source"], "data/feedface01234567");
        // Round trip.
        let parsed: TemplateResourceList = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, sample_resource_list());
    }

    #[test]
    fn import_template_request_wire_shape() {
        fixture_roundtrip::<ImportTemplateRequest>(
            r#"{"archive":"/tmp/base.dsh.tar.gz","name":null}"#,
        );
    }

    #[test]
    fn export_template_request_wire_shape() {
        fixture_roundtrip::<ExportTemplateRequest>(
            r#"{"name":"base.dsh","destination":"/tmp/out.tar.gz"}"#,
        );
    }

    #[test]
    fn remove_template_request_wire_shape() {
        fixture_roundtrip::<RemoveTemplateRequest>(r#"{"name":"base.dsh"}"#);
    }

    #[test]
    fn commit_image_request_wire_shape() {
        fixture_roundtrip::<CommitImageRequest>(
            r#"{"containerId":"container-1","outputPath":"/tmp/img","name":"demo","version":"0.1.0"}"#,
        );
    }

    #[test]
    fn load_image_request_wire_shape() {
        fixture_roundtrip::<LoadImageRequest>(r#"{"archivePath":"/tmp/demo.dshimage"}"#);
    }

    #[test]
    fn data_entry_wire_shape() {
        fixture_roundtrip::<DataEntry>(
            r#"{"name":"corpus","digest":"feedface","importedAt":1786897945,"source":"file:///tmp/corpus.tar.gz"}"#,
        );
    }

    #[test]
    fn data_use_wire_shape() {
        fixture_roundtrip::<DataUse>(r#"{"name":"corpus","digest":"feedface"}"#);
    }

    #[test]
    fn container_description_wire_shape() {
        // Minimal payload: `template`/`url`/`hostPid` default to absent,
        // `runtime` defaults to "host" (a record written before the field
        // existed is a host container), and `extensions` carries whatever
        // the daemon scanned (kept as a generic object on the wire).
        let json = r#"{
            "id": "container-1786941505",
            "name": "dsh-test",
            "version": "latest",
            "profile": "web",
            "directory": "/home/wpp/dshboxs/instances/container-1786941505",
            "status": "running",
            "runtime": "host",
            "extensions": {
                "containerId": "container-1786941505",
                "profiles": [],
                "skills": [],
                "diagnostics": [],
                "scannedAt": 1786900000
            }
        }"#;
        let parsed: ContainerDescription = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.id, "container-1786941505");
        assert_eq!(parsed.status, "running");
        assert_eq!(parsed.template, None);
        assert_eq!(parsed.url, None);
        assert_eq!(parsed.host_pid, None);
        // Round-trip: re-serialise and confirm the same JSON comes back.
        let rendered = serde_json::to_value(&parsed).unwrap();
        let expected: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(rendered, expected);
        // Running payload: optional fields populated.
        let full = r#"{
            "id": "container-1786941505",
            "name": "dsh-test",
            "version": "latest",
            "profile": "web",
            "template": "dsh-test",
            "directory": "/home/wpp/dshboxs/instances/container-1786941505",
            "status": "running",
            "runtime": "vm:firecracker",
            "url": "http://127.0.0.1:36847",
            "hostPid": 12345,
            "extensions": {}
        }"#;
        let parsed: ContainerDescription = serde_json::from_str(full).unwrap();
        assert_eq!(parsed.template.as_deref(), Some("dsh-test"));
        assert_eq!(parsed.url.as_deref(), Some("http://127.0.0.1:36847"));
        assert_eq!(parsed.host_pid, Some(12345));
        // The runtime string travels through this wire crate verbatim: it is
        // the daemon that validates it against the backends it can execute.
        assert_eq!(parsed.runtime, "vm:firecracker");
    }
