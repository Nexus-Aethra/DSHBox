//! Update discovery and installation primitives for DSH Box.
//!
//! The pieces here are pure: a version comparison, a release-manifest parser
//! with an asset selector, and a verifying downloader. Nothing in this crate
//! touches the network, the config file, or a process, so every rule that
//! decides *whether* an update may be applied is unit-testable without one.
//! The daemon owns the network and the desktop owns the process that has to
//! end; the decisions live here.

pub mod download;
pub mod manifest;
pub mod version;

pub use download::{download_and_verify, normalize_digest, DownloadError};
pub use manifest::{ManifestProblem, UpdateAsset, UpdateManifest, MANIFEST_SCHEMA};
pub use version::{is_newer, same_version, Version};

/// The GitHub repository this app releases from.
pub const REPOSITORY: &str = "Nexus-Aethra/DSHBox";

/// Where the newest-release metadata comes from.
///
/// This is the *quick* tier: one small JSON that says whether something newer
/// exists. It deliberately cannot install anything -- a tag says nothing about
/// which platform asset a machine needs, nor about its bytes. Installing needs
/// the `latest.json` asset that goes with the release.
pub const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/Nexus-Aethra/DSHBox/releases/latest";

/// Where the recent-release history comes from.
///
/// The version page lists the last few releases so a person can read what each
/// one changed before deciding anything. It is a read-only tier: history
/// informs, it never installs -- only the newest release carries a
/// `latest.json` manifest with digests.
pub const RELEASES_LIST_API: &str =
    "https://api.github.com/repos/Nexus-Aethra/DSHBox/releases?per_page=10";

/// The full manifest attached to a release tag.
pub fn latest_manifest_url(tag: &str) -> String {
    format!(
        "https://github.com/{REPOSITORY}/releases/download/{tag}/latest.json"
    )
}

/// The manifest always sits beside the installers of the release it describes.
pub const MANIFEST_ASSET_NAME: &str = "latest.json";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_url_names_the_release_tag_and_its_assets() {
        // The download URL and the manifest URL must agree on layout, or a
        // release publishes assets the app cannot find.
        let url = latest_manifest_url("v0.2.0");
        assert!(url.contains("releases/download/v0.2.0/latest.json"), "{url}");
        assert!(url.contains(REPOSITORY));
    }
}
