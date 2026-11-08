//! Streaming download with SHA-256 verification.
//!
//! Nothing this module produces is ever renamed into the place the installer
//! reads until the digest matches. That ordering is the whole point: a partial
//! or tampered transfer must leave the directory exactly as it found it, so a
//! half-written installer can never be mistaken for a ready one.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// Why a download stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /// The transfer itself failed.
    Transport(String),
    /// The bytes arrived but their digest is not the manifest's.
    Digest { expected: String, actual: String },
    /// The digest in the manifest is not a SHA-256 hex string, so it could
    /// never have matched anything.
    MalformedDigest(String),
    /// The size disagrees with the manifest.
    Size { expected: u64, actual: u64 },
    /// The caller asked to stop; the transfer ends mid-stream and the
    /// staging file is removed with it.
    Cancelled,
}

impl DownloadError {
    pub fn message(&self) -> String {
        match self {
            Self::Transport(detail) => format!("download failed: {detail}"),
            Self::Digest { expected, actual } => format!(
                "the downloaded file does not match the release digest (expected {expected}, got {actual})"
            ),
            Self::MalformedDigest(digest) => format!(
                "the release manifest carries a malformed SHA-256 digest: {digest:?}"
            ),
            Self::Size { expected, actual } => format!(
                "the downloaded file is {actual} bytes; the release says {expected}"
            ),
            Self::Cancelled => "download cancelled".to_owned(),
        }
    }
}

/// Normalize a digest from a manifest to lower-case, and refuse anything that
/// is not 64 hex characters.
///
/// GitHub writes `sha256:<hex>` in its release API and a bare hex string in
/// most manifests; both are accepted, so the two sources cannot disagree about
/// what a digest looks like.
pub fn normalize_digest(value: &str) -> Result<String, DownloadError> {
    let trimmed = value.trim();
    let hex = trimmed
        .strip_prefix("sha256:")
        .unwrap_or(trimmed)
        .to_ascii_lowercase();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(DownloadError::MalformedDigest(trimmed.to_owned()));
    }
    Ok(hex)
}

/// Incremental SHA-256, so a 160 MB installer never has to sit in memory to be
/// checked.
#[derive(Default)]
pub struct StreamingDigest {
    hasher: Sha256,
    bytes: u64,
}

impl StreamingDigest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Consume the hasher and return the hex digest.
    pub fn finish(self) -> String {
        format!("{:x}", self.hasher.finalize())
    }
}

/// Copy a reader to a path while hashing, then check what landed against the
/// manifest.
///
/// The caller passes a `destination` that is a staging path; this function
/// removes it on any mismatch -- and on a mid-transfer cancellation, which
/// must not leave a half-written installer behind either -- so a failed or
/// abandoned attempt cannot leave a file that a later run would find and
/// trust. `should_stop` is consulted between chunks: cancellation stops the
/// transfer within one buffer, not after it.
pub fn download_and_verify(
    mut reader: impl Read,
    destination: &std::path::Path,
    expected_digest: &str,
    expected_size: u64,
    mut progress: impl FnMut(u64, u64),
    should_stop: impl Fn() -> bool,
) -> Result<u64, DownloadError> {
    let expected = normalize_digest(expected_digest)?;
    let parent = destination
        .parent()
        .ok_or_else(|| DownloadError::Transport("staging path has no parent".to_owned()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| DownloadError::Transport(format!("cannot create staging directory: {error}")))?;
    // A stale staging file from an earlier attempt must not be resumed or
    // appended to: this function writes a whole file or none.
    let _ = std::fs::remove_file(destination);
    let mut file = std::fs::File::create(destination)
        .map_err(|error| DownloadError::Transport(format!("cannot create staging file: {error}")))?;
    let mut digest = StreamingDigest::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| DownloadError::Transport(format!("read failed: {error}")))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| DownloadError::Transport(format!("write failed: {error}")))?;
        progress(digest.bytes(), expected_size);
        if should_stop() {
            let _ = std::fs::remove_file(destination);
            return Err(DownloadError::Cancelled);
        }
    }
    drop(file);
    let actual_bytes = digest.bytes();
    let actual = digest.finish();
    if expected_size != 0 && actual_bytes != expected_size {
        let _ = std::fs::remove_file(destination);
        return Err(DownloadError::Size {
            expected: expected_size,
            actual: actual_bytes,
        });
    }
    if actual != expected {
        let _ = std::fs::remove_file(destination);
        return Err(DownloadError::Digest {
            expected,
            actual,
        });
    }
    Ok(actual_bytes)
}

#[cfg(test)]
mod tests {
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
}
