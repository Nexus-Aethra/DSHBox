//! Local image registry: the metadata-only layer between `build` and
//! container creation (see `docs/specs/image-build.md`).
//!
//! An image is a single `list.json` under `images/<fnv1a64>/` plus one row
//! in `images/index.json` — the exact same content-addressed layout the
//! template store uses. The list never embeds resource content:
//!
//! * `plugin` resources are **references** to repository entries
//!   (`entryId` + `version`) — build does not touch their content;
//! * every other kind is a **snapshot**: a `name -> digest` mapping into
//!   the global data store (`data/<digest>/`), hard-copied into each
//!   container at creation time.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Bump together with any structural change to [`ImageList`].
pub const IMAGE_LIST_SCHEMA_VERSION: u32 = 7;

/// One resource row inside an image list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "mode")]
pub enum ImageResource {
    /// A plugin recorded as a reference into the shared repository.
    #[serde(rename_all = "camelCase")]
    Reference {
        kind: String,
        name: String,
        version: Option<String>,
        entry_id: String,
    },
    /// Any non-plugin kind recorded as a content-addressed snapshot of the
    /// global data store.
    #[serde(rename_all = "camelCase")]
    Snapshot {
        kind: String,
        name: String,
        digest: String,
        destination: String,
    },
}

/// The complete metadata of one image — the only thing an image owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageList {
    pub schema_version: u32,
    pub name: String,
    /// The template (or harness ref) this image was built from.
    pub base: String,
    pub profile: String,
    pub harness_ref: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub created_at: u64,
    pub resources: Vec<ImageResource>,
}

/// One row of `images/index.json`, mirroring the template index entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageEntry {
    pub name: String,
    pub id: String,
    pub base: String,
    pub created_at: u64,
}

pub type ImageIndex = BTreeMap<String, ImageEntry>;

pub fn images_root(root: &str) -> PathBuf {
    PathBuf::from(root).join("images")
}

pub fn image_index_path(root: &str) -> PathBuf {
    PathBuf::from(root).join("state/image-index.json")
}

pub fn image_list_path(root: &str, id: &str) -> PathBuf {
    images_root(root).join(id).join("list.json")
}

pub fn read_image_index(root: &str) -> Result<ImageIndex, String> {
    let path = image_index_path(root);
    if !path.is_file() {
        return Ok(ImageIndex::new());
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

pub fn write_image_index(root: &str, index: &ImageIndex) -> Result<(), String> {
    let path = image_index_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let body = serde_json::to_string_pretty(index).map_err(|error| error.to_string())?;
    fs::write(&path, body).map_err(|error| error.to_string())
}

/// Content hash of a serialised list — the image id (fnv1a64 hex, the same
/// algorithm as template storage).
pub fn image_list_hash(list: &ImageList) -> Result<String, String> {
    let body = serde_json::to_string(list).map_err(|error| error.to_string())?;
    Ok(box_dsh_versions::template_content_hash(&body))
}

/// Persist an image: write `images/<id>/list.json`, update the index
/// (same-name overwrite retires the previous id), and GC a retired hash
/// directory nobody references any more. Returns the stored entry.
pub fn write_image(root: &str, list: &ImageList) -> Result<ImageEntry, String> {
    let id = image_list_hash(list)?;
    let previous = read_image_index(root)?;
    let retired_id = previous.get(&list.name).map(|entry| entry.id.clone());

    let directory = images_root(root).join(&id);
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let body = serde_json::to_string_pretty(list).map_err(|error| error.to_string())?;
    fs::write(directory.join("list.json"), body).map_err(|error| error.to_string())?;

    let entry = ImageEntry {
        name: list.name.clone(),
        id: id.clone(),
        base: list.base.clone(),
        created_at: list.created_at,
    };
    let mut index = previous;
    index.insert(list.name.clone(), entry.clone());
    write_image_index(root, &index)?;

    // Retire the hash directory a same-name rebuild replaced — unless the
    // content happens to be identical (then both names point at one id).
    if let Some(retired) = retired_id {
        if retired != id {
            collect_unreferenced_image(root, &retired, &index);
        }
    }
    Ok(entry)
}

/// Read one image list by index name.
pub fn read_image_by_name(root: &str, name: &str) -> Result<Option<ImageList>, String> {
    let Some(entry) = read_image_index(root)?.remove(name) else {
        return Ok(None);
    };
    read_image_by_id(root, &entry.id)
}

pub fn read_image_by_id(root: &str, id: &str) -> Result<Option<ImageList>, String> {
    let path = image_list_path(root, id);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("cannot parse {}: {error}", path.display()))
}

/// Remove an image from the index and GC its hash directory when no other
/// index row points at it. Refusal when containers still use the image is
/// the caller's job (it needs the container registry).
pub fn remove_image(root: &str, name: &str) -> Result<bool, String> {
    let mut index = read_image_index(root)?;
    let Some(entry) = index.remove(name) else {
        return Ok(false);
    };
    write_image_index(root, &index)?;
    collect_unreferenced_image(root, &entry.id, &index);
    Ok(true)
}

/// Delete `images/<id>/` when no index row references the id.
pub fn collect_unreferenced_image(root: &str, id: &str, index: &ImageIndex) {
    if index.values().any(|entry| entry.id == id) {
        return;
    }
    let _ = fs::remove_dir_all(images_root(root).join(id));
}

/// Every digest referenced by any stored image — the input to
/// `image prune` on the daemon side (data store GC).
pub fn referenced_snapshot_digests(root: &str) -> Result<Vec<String>, String> {
    let mut digests = Vec::new();
    for entry in read_image_index(root)?.values() {
        if let Some(list) = read_image_by_id(root, &entry.id)? {
            for resource in &list.resources {
                if let ImageResource::Snapshot { digest, .. } = resource {
                    digests.push(digest.clone());
                }
            }
        }
    }
    digests.sort();
    digests.dedup();
    Ok(digests)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-image-reg-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_list(name: &str) -> ImageList {
        ImageList {
            schema_version: IMAGE_LIST_SCHEMA_VERSION,
            name: name.to_owned(),
            base: "github.com/deepseek-ai/deepseek-harness:latest".to_owned(),
            profile: "web".to_owned(),
            harness_ref: Some("latest".to_owned()),
            labels: BTreeMap::new(),
            created_at: 1_786_900_000,
            resources: vec![
                ImageResource::Reference {
                    kind: "plugin".to_owned(),
                    name: "dsh-better-sidebar".to_owned(),
                    version: Some("0.12.2".to_owned()),
                    entry_id: "a1b2c3d4".to_owned(),
                },
                ImageResource::Snapshot {
                    kind: "skill".to_owned(),
                    name: "boxfile-guide".to_owned(),
                    digest: "feedface01234567".to_owned(),
                    destination: "profile/skills/boxfile-guide".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn write_then_read_round_trip() {
        let root = sandbox("roundtrip");
        let entry = write_image(root.to_str().unwrap(), &sample_list("demo")).unwrap();
        assert_eq!(entry.name, "demo");
        assert_eq!(entry.id.len(), 16);

        let loaded = read_image_by_name(root.to_str().unwrap(), "demo")
            .unwrap()
            .expect("image must exist");
        assert_eq!(loaded, sample_list("demo"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn same_name_rebuild_retires_old_hash() {
        let root = sandbox("retire");
        let first = write_image(root.to_str().unwrap(), &sample_list("demo")).unwrap();
        let mut changed = sample_list("demo");
        changed.resources.push(ImageResource::Snapshot {
            kind: "data".to_owned(),
            name: "corpus".to_owned(),
            digest: "0123feedface4567".to_owned(),
            destination: "data/corpus".to_owned(),
        });
        let second = write_image(root.to_str().unwrap(), &changed).unwrap();
        assert_ne!(first.id, second.id);
        // The retired hash directory is gone; the new one exists.
        assert!(!image_list_path(root.to_str().unwrap(), &first.id).is_file());
        assert!(image_list_path(root.to_str().unwrap(), &second.id).is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_gcs_hash_directory() {
        let root = sandbox("remove");
        let entry = write_image(root.to_str().unwrap(), &sample_list("demo")).unwrap();
        assert!(remove_image(root.to_str().unwrap(), "demo").unwrap());
        assert!(!images_root(root.to_str().unwrap()).join(&entry.id).exists());
        assert!(read_image_index(root.to_str().unwrap()).unwrap().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn referenced_digests_cover_every_snapshot() {
        let root = sandbox("digests");
        write_image(root.to_str().unwrap(), &sample_list("a")).unwrap();
        let mut other = sample_list("b");
        other.resources.push(ImageResource::Snapshot {
            kind: "data".to_owned(),
            name: "corpus".to_owned(),
            digest: "0123feedface4567".to_owned(),
            destination: "data/corpus".to_owned(),
        });
        write_image(root.to_str().unwrap(), &other).unwrap();
        let digests = referenced_snapshot_digests(root.to_str().unwrap()).unwrap();
        assert_eq!(digests, vec!["0123feedface4567", "feedface01234567"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn wire_shape_matches_spec() {
        // The spec (docs/specs/image-build.md) pins this JSON layout; any
        // field change must update the spec deliberately.
        let json = serde_json::to_value(sample_list("demo")).unwrap();
        assert_eq!(json["schemaVersion"], 7);
        assert_eq!(json["harnessRef"], "latest");
        let plugin = &json["resources"][0];
        assert_eq!(plugin["mode"], "reference");
        assert_eq!(plugin["entryId"], "a1b2c3d4");
        let skill = &json["resources"][1];
        assert_eq!(skill["mode"], "snapshot");
        assert_eq!(skill["digest"], "feedface01234567");
    }
}
