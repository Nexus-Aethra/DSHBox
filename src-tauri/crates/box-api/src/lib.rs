//! Wire types shared between `dshboxd` and every client (desktop Tauri
//! commands, the `dshbox` CLI).
//!
//! Before this crate existed, the daemon and the desktop layer each kept a
//! hand-copied definition of these structs; a field added on one side
//! (e.g. the hash-storage `id` replacing `path`) silently turned a
//! successful RPC into a deserialization error and the UI showed an empty
//! list while the CLI kept working. With a single definition, any drift is
//! a compile error, and the fixture tests below pin the JSON shape so a
//! rename cannot sneak past unnoticed.

use serde::{Deserialize, Serialize};

/// One local template as returned by `list_templates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInfo {
    pub name: String,
    /// Content-addressable identifier (fnv1a64 hex of the script body).
    pub id: String,
    pub harness_ref: Option<String>,
    pub profile: String,
}

/// Raw text of a template as returned by `read_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateText {
    pub name: String,
    pub text: String,
}

/// Parameters of the `build_image` task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildImageRequest {
    pub script_path: String,
    pub output_path: Option<String>,
    pub container_name: Option<String>,
}

/// Parameters of `create_container_from_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTemplateContainerRequest {
    pub name: String,
    pub template: String,
    pub profile: Option<String>,
}

/// Parameters of `create_container_from_image`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateImageContainerRequest {
    pub name: String,
    pub image: String,
}

/// Parameters of `import_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTemplateRequest {
    pub archive: String,
    pub name: Option<String>,
}

/// Parameters of `export_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTemplateRequest {
    pub name: String,
    pub destination: Option<String>,
}

/// Parameters of `remove_template`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoveTemplateRequest {
    pub name: String,
}

/// Stub payload for the planned `image commit` flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitImageRequest {
    pub container_id: String,
    pub output_path: String,
    pub name: String,
    pub version: String,
}

/// Stub payload for the planned `image load` flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadImageRequest {
    pub archive_path: String,
}

/// One entry of the content-addressed data store. Doubles as the on-disk
/// index format (`<root>/data/index.json`), so treat field renames as a
/// storage migration, not just a wire change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataEntry {
    pub name: String,
    pub digest: String,
    pub imported_at: u64,
    pub source: String,
}

/// One data payload copied into a container; recorded in
/// `<container>/state/data.json` so `image prune` knows which digests are
/// still in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataUse {
    pub name: String,
    pub digest: String,
}

#[cfg(test)]
mod tests {
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
            r#"{"name":"github.com/deepseek-ai/deepseek-harness:latest","id":"a1b2c3d4e5f60718","harnessRef":"latest","profile":"web"}"#,
        );
        // `harnessRef` may be absent for imported templates.
        let parsed: TemplateInfo = serde_json::from_str(
            r#"{"name":"local.dsh","id":"0000000000000001","profile":"web"}"#,
        )
        .unwrap();
        assert_eq!(parsed.harness_ref, None);
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

    #[test]
    fn create_image_container_request_wire_shape() {
        fixture_roundtrip::<CreateImageContainerRequest>(
            r#"{"name":"demo","image":"sidebar-demo"}"#,
        );
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
}
