//! tests for download.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::io::Cursor;

    const HELLO: &[u8] = b"the bytes CI built";

    fn hello_digest() -> String {
        let mut digest = StreamingDigest::new();
        digest.update(HELLO);
        digest.finish()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "dshbox-updates-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn digest_normalization_accepts_both_spellings() {
        let bare = "96330EED2B584A976925EA77AB7BDA52A1CFA3A26E3EC8B1820DABDCCC497B60";
        assert_eq!(normalize_digest(bare).unwrap(), bare.to_ascii_lowercase());
        assert_eq!(
            normalize_digest(&format!("sha256:{bare}")).unwrap(),
            bare.to_ascii_lowercase()
        );
        assert!(normalize_digest("nope").is_err());
        assert!(normalize_digest(&"z".repeat(64)).is_err());
    }

    #[test]
    fn a_matching_transfer_lands_its_bytes() {
        let directory = scratch("match");
        let path = directory.join("installer.bin");
        let digest = hello_digest();
        let written = download_and_verify(
            Cursor::new(HELLO),
            &path,
            &digest,
            HELLO.len() as u64,
            |_, _| {},
            || false,
        )
        .unwrap();
        assert_eq!(written, HELLO.len() as u64);
        assert_eq!(std::fs::read(&path).unwrap(), HELLO);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_wrong_digest_removes_the_file_it_just_wrote() {
        // The failure this guards: a mismatched file left on disk is one a
        // later run would find and trust.
        let directory = scratch("mismatch");
        let path = directory.join("installer.bin");
        let wrong = "0".repeat(64);
        let error = download_and_verify(
            Cursor::new(HELLO),
            &path,
            &wrong,
            0,
            |_, _| {},
            || false,
        )
        .unwrap_err();
        assert!(matches!(error, DownloadError::Digest { .. }));
        assert!(!path.exists(), "a failed verification must leave nothing behind");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_size_disagreement_is_a_failure_even_with_a_right_digest() {
        // A manifest that names a different size is describing a different
        // artifact; trusting the digest alone would hide that.
        let directory = scratch("size");
        let path = directory.join("installer.bin");
        let digest = hello_digest();
        let error = download_and_verify(
            Cursor::new(HELLO),
            &path,
            &digest,
            HELLO.len() as u64 + 1,
            |_, _| {},
            || false,
        )
        .unwrap_err();
        assert!(matches!(error, DownloadError::Size { .. }));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_malformed_manifest_digest_is_refused_before_writing_anything() {
        let directory = scratch("malformed");
        let path = directory.join("installer.bin");
        let error =
            download_and_verify(Cursor::new(HELLO), &path, "junk", 0, |_, _| {}, || false)
                .unwrap_err();
        assert!(matches!(error, DownloadError::MalformedDigest(_)));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn progress_reports_growing_byte_counts() {
        let directory = scratch("progress");
        let path = directory.join("installer.bin");
        let digest = hello_digest();
        let mut seen = Vec::new();
        download_and_verify(
            Cursor::new(HELLO),
            &path,
            &digest,
            HELLO.len() as u64,
            |bytes, _| seen.push(bytes),
            || false,
        )
        .unwrap();
        assert_eq!(seen.last().copied(), Some(HELLO.len() as u64));
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_cancelled_transfer_stops_and_leaves_nothing_behind() {
        // Cancel must bite between chunks, not after the whole body: a
        // cancelled 160 MB download that keeps running is not cancelled.
        let directory = scratch("cancel");
        let path = directory.join("installer.bin");
        let digest = hello_digest();
        let error = download_and_verify(
            Cursor::new(HELLO),
            &path,
            &digest,
            0,
            |_, _| {},
            || true,
        )
        .unwrap_err();
        assert!(matches!(error, DownloadError::Cancelled));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }
