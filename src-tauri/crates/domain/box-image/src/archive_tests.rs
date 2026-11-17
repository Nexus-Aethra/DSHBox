//! tests for archive.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

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
