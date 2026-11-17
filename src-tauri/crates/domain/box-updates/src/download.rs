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
#[path = "download_tests.rs"]
mod tests;
