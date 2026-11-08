//! Gzip tar writer / reader for dshimage archives.
//!
//! Layout on disk:
//!
//! ```text
//! manifest.json
//! blobs/
//!   <digest>/source/<original files...>
//! ```
//!
//! Blobs are keyed by content digest (fnv1a64) so two images that include
//! the same private plugin share storage on disk. The reader extracts into
//! a caller-provided staging directory and refuses paths that escape it.
//!
//! Digests use `<algo>:<hex>` form, but `:` is not a valid path character
//! on Windows. Both the writer and reader map the colon to an underscore
//! before touching the filesystem so a tar round-trip works on every host.
//! The `manifest.json` keeps the original digest form (logical path); the
//! sanitisation is purely an FS-side concern.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use tar::{Archive, Builder};

use crate::error::ImageError;
use crate::manifest::{write_manifest_to, ImageManifest};

/// In-memory handle produced by `read_dshimage`. The `staging_root` points
/// at the directory the archive was unpacked into; callers are expected to
/// keep it alive until they've consumed everything they need.
#[derive(Debug)]
pub struct ImageArchive {
    pub manifest: ImageManifest,
    pub staging_root: PathBuf,
}

/// Map a digest to a filesystem-safe directory name. Replaces `:`
/// (the conventional algorithm/value separator) with `_` because NTFS
/// rejects colons in path components. The mapping is symmetric: see
/// [`digest_from_blob_dir`].
pub(crate) fn safe_blob_dir(digest: &str) -> String {
    digest.replace(':', "_")
}

/// Inverse of [`safe_blob_dir`]; `safe` -> `orig` so callers that already
/// know they read a sanitised path can recover the canonical digest.
#[allow(dead_code)]
pub(crate) fn digest_from_blob_dir(safe: &str) -> String {
    safe.replace('_', ":")
}

/// Write `manifest` plus the supplied blobs into `out`. Each tuple is
/// `(digest, source_root)`; the source root is copied into
/// `blobs/<safe-digest>/source/`, skipping `.git` and `node_modules`.
pub fn write_dshimage(
    manifest: &ImageManifest,
    blobs: &[(String, &Path)],
    out: &Path,
) -> Result<(), ImageError> {
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = File::create(out)?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(encoder);

    // manifest.json first so readers find it as the entry point.
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    append_bytes(&mut builder, "manifest.json", &manifest_bytes)?;

    for (digest, source_root) in blobs {
        let target = PathBuf::from("blobs").join(safe_blob_dir(digest)).join("source");
        append_dir_skipping_noise(&mut builder, source_root, &target)?;
    }

    builder.finish()?;
    let encoder = builder.into_inner().map_err(|error| {
        ImageError::Io(std::io::Error::other(format!("cannot finalize tar: {error}")))
    })?;
    encoder.finish()?;
    Ok(())
}

fn append_bytes<W: std::io::Write>(
    builder: &mut Builder<W>,
    name: &str,
    bytes: &[u8],
) -> Result<(), ImageError> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .map_err(ImageError::Io)?;
    Ok(())
}

fn append_dir_skipping_noise<W: std::io::Write>(
    builder: &mut Builder<W>,
    source_root: &Path,
    target_root: &Path,
) -> Result<(), ImageError> {
    if !source_root.is_dir() {
        return Err(ImageError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "blob source `{}` is not a directory",
                source_root.display()
            ),
        )));
    }
    builder
        .append_dir(target_root, source_root)
        .map_err(ImageError::Io)?;
    visit_dir(builder, source_root, target_root)
}

fn visit_dir<W: std::io::Write>(
    builder: &mut Builder<W>,
    source_root: &Path,
    target_root: &Path,
) -> Result<(), ImageError> {
    for entry in std::fs::read_dir(source_root)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some(".git" | "node_modules" | "dist" | "build" | ".cache" | ".dsh")
        ) {
            continue;
        }
        let path = entry.path();
        let target = target_root.join(&name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            builder.append_dir(&target, &path).map_err(ImageError::Io)?;
            visit_dir(builder, &path, &target)?;
        } else if file_type.is_file() {
            builder
                .append_path_with_name(&path, &target)
                .map_err(ImageError::Io)?;
        }
    }
    Ok(())
}

/// Read a dshimage archive into `staging`. Returns the parsed manifest and
/// the staging directory the blobs were unpacked into.
pub fn read_dshimage(archive: &Path, staging: &Path) -> Result<ImageArchive, ImageError> {
    if !archive.is_file() {
        return Err(ImageError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("archive `{}` does not exist", archive.display()),
        )));
    }
    if staging.exists() {
        std::fs::remove_dir_all(staging)?;
    }
    std::fs::create_dir_all(staging)?;

    let file = File::open(archive)?;
    let lower = archive
        .to_string_lossy()
        .to_ascii_lowercase();
    let mut tar: Box<dyn Read> = if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Box::new(GzDecoder::new(file))
    } else if lower.ends_with(".tar") {
        Box::new(file)
    } else {
        return Err(ImageError::InvalidManifest(format!(
            "archive must be .tar or .tar.gz (got {})",
            archive.display()
        )));
    };

    let mut manifest: Option<ImageManifest> = None;
    let mut tar_archive = Archive::new(&mut tar);
    for entry in tar_archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let raw = path.to_string_lossy().to_string();
        if raw == "manifest.json" || raw == "./manifest.json" {
            let mut buffer = String::new();
            entry.read_to_string(&mut buffer)?;
            manifest = Some(crate::manifest::parse_manifest(&buffer)?);
            continue;
        }
        // Reject path traversal attempts.
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(ImageError::InvalidManifest(format!(
                "archive contains unsafe path `{}`",
                path.display()
            )));
        }
        // Reject entries that still carry a colon — the writer sanitises
        // digest directories to underscores; an archive containing `:` is
        // either pre-fix legacy (we refuse rather than silently corrupt) or
        // malformed. The Windows test for `:` in path components would
        // otherwise fail late inside `create_dir_all`.
if raw.contains(':') {
            return Err(ImageError::InvalidManifest(format!(
                "archive contains path with reserved character `:` (`{raw}`); rebuild with a fixed writer"
            )));
        }
        entry.unpack_in(staging)?;
    }

    let manifest = match manifest {
        Some(manifest) => manifest,
        None => return Err(ImageError::ArchiveMissingManifest(archive.to_path_buf())),
    };

    Ok(ImageArchive {
        manifest,
        staging_root: staging.to_path_buf(),
    })
}

/// Convenience: persist a manifest to a temporary file inside `staging`
/// for round-trip tests / debugging.
#[allow(dead_code)]
pub fn dump_manifest(staging: &Path, manifest: &ImageManifest) -> Result<PathBuf, ImageError> {
    let path = staging.join("manifest.json");
    write_manifest_to(&path, manifest)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::compile_manifest;
    use crate::script::{parse_script, ImageOp, ParsedSource};
    use std::collections::BTreeMap;

    fn sandbox(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dshbox-image-archive-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn build_script() -> crate::script::ImageScript {
        parse_script(
            "FROM dsh:latest\nPROFILE web\nNAME roundtrip\nADD plugin cordis-plugin-bar\n",
            &PathBuf::from("/tmp"),
        )
        .unwrap()
    }

    #[test]
    fn round_trips_manifest_only() {
        let staging = sandbox("manifest-only");
        let manifest = compile_manifest(&build_script(), 1);
        let archive_path = staging.join("plain.tar.gz");
        write_dshimage(&manifest, &[], &archive_path).unwrap();
        let restored = read_dshimage(&archive_path, &staging.join("out")).unwrap();
        assert_eq!(restored.manifest, manifest);
        let _ = std::fs::remove_dir_all(staging);
    }

    #[test]
    fn writes_and_reads_blob() {
        let staging = sandbox("with-blob");
        let blob_root = staging.join("source");
        std::fs::create_dir_all(&blob_root).unwrap();
        std::fs::write(blob_root.join("SKILL.md"), "---\nname: demo\n---\n").unwrap();

        let mut manifest = compile_manifest(&build_script(), 1);
        // Add a Skill add pointing at the blob we just wrote.
        manifest.adds.push(crate::manifest::ResolvedAdd {
            kind: crate::script::AddKind::Skill,
            source: crate::manifest::AddSource::LocalPath {
                path: blob_root.to_string_lossy().into_owned(),
            },
            resource_type: crate::manifest::ResourceType::Code,
            destination: "profile/skills/demo".to_owned(),
            blob: "blobs/fnv1a64:0000000000000000/source".to_owned(),
            digest: "fnv1a64:0000000000000000".to_owned(),
        });

        let archive_path = staging.join("with-blob.tar.gz");
        write_dshimage(&manifest, &[("fnv1a64:0000000000000000".to_string(), &blob_root)], &archive_path).unwrap();

        let out_staging = staging.join("out");
        let restored = read_dshimage(&archive_path, &out_staging).unwrap();
        // Digests are stored on disk using `_` in place of `:` because
        // NTFS rejects colons; the manifest keeps the logical `algo:hex`
        // form for human-readable diffs.
        let blob_unpacked = out_staging.join("blobs/fnv1a64_0000000000000000/source/SKILL.md");
        assert!(blob_unpacked.is_file(), "blob was not unpacked at {blob_unpacked:?}");
        assert_eq!(restored.manifest.adds.len(), manifest.adds.len());
        let _ = std::fs::remove_dir_all(staging);
    }

    #[test]
    fn script_holds_bare_name_entry() {
        // Sanity check that the script parser still tags bare-name entries.
        let script = build_script();
        match &script.ops[0] {
            ImageOp::Add { kind, source, .. } => {
                assert!(matches!(kind, crate::script::AddKind::Plugin));
                assert!(matches!(
                    source,
                    ParsedSource::BareName { ref name, .. } if name == "cordis-plugin-bar"
                ));
            }
        }
        // Touch `BTreeMap` import above so unused-import warnings stay quiet.
        let _ = BTreeMap::<String, String>::new();
    }
}
