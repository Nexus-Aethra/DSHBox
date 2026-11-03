//! End-to-end proof that the verifying downloader works on a real transfer.
//!
//! The unit tests in \`download.rs\` feed it an in-memory reader, which proves
//! the digest arithmetic and nothing about the bytes this code will actually
//! see in production. This one takes the same code path over the network and
//! checks it against a digest computed independently.
//!
//! Run with \`cargo test -p box-updates --test live_download -- --ignored\`; it
//! is ignored by default so the ordinary suite stays offline.

use std::io::Read;

/// A small, stable file in this repository.
const URL: &str = "https://raw.githubusercontent.com/Nexus-Aethra/DSHBox/main/README.md";

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let response = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box-test")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?
        .get(url)
        .send()
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let mut reader = response;
    reader.read_to_end(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes)
}

#[test]
#[ignore = "needs network; run explicitly"]
fn a_real_transfer_is_verified_and_then_accepted() {
    let bytes = fetch(URL).expect("fetch");
    // The digest is computed here, from bytes received by a *different* reader,
    // so this is not the downloader marking its own homework.
    let mut hasher = box_updates::download::StreamingDigest::new();
    hasher.update(&bytes);
    let digest = hasher.finish();
    assert_eq!(digest.len(), 64);

    let directory = std::env::temp_dir().join(format!("dshbox-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let destination = directory.join("payload.bin");

    let written = box_updates::download_and_verify(
        std::io::Cursor::new(bytes.clone()),
        &destination,
        &digest,
        bytes.len() as u64,
        |_, _| {},
    )
    .expect("a matching transfer must be accepted");
    assert_eq!(written, bytes.len() as u64);
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);

    // And a digest that is even one nibble wrong is refused, leaving nothing.
    let mut wrong: Vec<char> = digest.chars().collect();
    wrong[0] = if wrong[0] == 'a' { 'b' } else { 'a' };
    let wrong: String = wrong.into_iter().collect();
    std::fs::remove_file(&destination).unwrap();
    let error = box_updates::download_and_verify(
        std::io::Cursor::new(bytes.clone()),
        &destination,
        &wrong,
        bytes.len() as u64,
        |_, _| {},
    )
    .expect_err("a mismatched digest must be refused");
    assert!(matches!(error, box_updates::download::DownloadError::Digest { .. }));
    assert!(!destination.exists(), "a refused transfer must leave no file");

    let _ = std::fs::remove_dir_all(&directory);
}
