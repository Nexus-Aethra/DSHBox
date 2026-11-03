//! The `latest.json` release manifest: parsing, schema guard, and asset
//! selection for this machine.
//!
//! The manifest is what makes "the bytes I downloaded are the bytes CI
//! built" checkable after the transfer, from a value a person can read.
//! So it is parsed strictly: an unknown schema is an error rather than a
//! best-effort read, because a partially understood manifest is exactly the
//! shape that would install the wrong file.

use serde::{Deserialize, Serialize};

/// The only schema this build understands.
pub const MANIFEST_SCHEMA: u32 = 1;

/// One downloadable installer for one platform.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAsset {
    /// Absolute URL as published. The GitHub mirror is applied at request
    /// time, never baked in here.
    pub url: String,
    /// Lower-case hex SHA-256 of the bytes at `url`.
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
    /// `msi` | `deb` | `rpm` | `dmg`
    pub installer: String,
    /// `auto` when the box can install it unattended, `manual` when it
    /// needs a person (every Linux package, which needs root).
    #[serde(default = "default_install_mode")]
    pub install_mode: String,
}

fn default_install_mode() -> String {
    "manual".to_owned()
}

impl UpdateAsset {
    /// Installs without a person involved?
    pub fn is_automatic(&self) -> bool {
        self.install_mode == "auto"
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateManifest {
    pub schema: u32,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes_url: Option<String>,
    /// Oldest build that may take this update in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_version: Option<String>,
    /// Platform key -> its installers. An array because Linux publishes
    /// both a `.deb` and a `.rpm` for the same machine; one installer for a
    /// platform is an array of one.
    #[serde(default)]
    pub platforms: std::collections::BTreeMap<String, Vec<UpdateAsset>>,
}

/// A manifest that parsed but cannot be used, with the reason in wire terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestProblem {
    /// The schema number is not one this build understands.
    UnknownSchema(u32),
    /// No entry for this machine's platform directory.
    NoPlatform(String),
}

impl ManifestProblem {
    /// The `reason` string the UI and the wire surface report.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::UnknownSchema(_) => "unknown-schema",
            Self::NoPlatform(_) => "no-asset",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Self::UnknownSchema(schema) => format!(
                "the release manifest uses schema {schema}; this build understands {MANIFEST_SCHEMA}"
            ),
            Self::NoPlatform(target) => format!(
                "the release has no installer for this machine ({target})"
            ),
        }
    }
}

impl UpdateManifest {
    /// Parse JSON into a manifest, refusing a schema this build does not know.
    pub fn parse(text: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(text)
            .map_err(|error| format!("cannot parse the release manifest: {error}"))?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(ManifestProblem::UnknownSchema(manifest.schema).detail());
        }
        Ok(manifest)
    }

    /// The installers published for this machine, if any.
    pub fn assets_for(&self, target: &str) -> Result<&[UpdateAsset], ManifestProblem> {
        match self.platforms.get(target) {
            Some(assets) if !assets.is_empty() => Ok(assets),
            _ => Err(ManifestProblem::NoPlatform(target.to_owned())),
        }
    }

    /// Pick the installer to use on this machine.
    ///
    /// A platform that publishes several (deb and rpm) is resolved by the
    /// platform's own preference order, which is the order CI wrote them in:
    /// the first entry is the one this platform prefers. Choosing by file
    /// size or by name would be guessing at which one the package manager
    /// can actually consume.
    pub fn preferred_asset(&self, target: &str, preferred_installers: &[&str])
        -> Result<UpdateAsset, ManifestProblem>
    {
        let assets = self.assets_for(target)?;
        if let Some(found) = preferred_installers.iter().find_map(|installer| {
            assets.iter().find(|asset| asset.installer == *installer)
        }) {
            return Ok(found.clone());
        }
        Ok(assets[0].clone())
    }

    /// Is this update allowed to be installed in place by `current`?
    pub fn installable_from(&self, current: &str) -> bool {
        match self.minimum_version.as_deref() {
            // No floor stated: the release is suitable for anything that
            // can parse it.
            None => true,
            Some(minimum) => crate::version::is_newer(current, minimum)
                || crate::version::same_version(current, minimum),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A syntactically valid digest; the tests here exercise parsing and
    /// selection, not hashing (that is `download.rs`).
    const DIGEST: &str =
        "96330eed2b584a976925ea77ab7bda52a1cfa3a26e3ec8b1820dabdccc497b60";

    fn sample(platforms: &str) -> String {
        format!(
            r#"{{"schema":1,"version":"0.2.0","minimumVersion":"0.1.9","platforms":{platforms}}}"#
        )
    }

    #[test]
    fn a_manifest_this_build_understands_parses() {
        let text = sample(&format!(
            r#"{{"win-x64":[{{"url":"https://example.test/a.msi","sha256":"{DIGEST}",
                "installer":"msi","installMode":"auto"}}]}}"#
        ));
        let parsed = UpdateManifest::parse(&text).unwrap();
        assert_eq!(parsed.version, "0.2.0");
        assert_eq!(parsed.minimum_version.as_deref(), Some("0.1.9"));
    }

    #[test]
    fn an_unknown_schema_is_an_error_not_a_best_effort_read() {
        // A partially understood manifest is how the wrong file gets run.
        let text = r#"{"schema":99,"version":"9.0.0","platforms":{}}"#;
        let error = UpdateManifest::parse(text).unwrap_err();
        assert!(error.contains("schema 99"), "{error}");
    }

    #[test]
    fn a_preferred_installer_wins_over_publish_order() {
        // An array per platform, in CI's publish order: the rpm first.
        // Swapping the order would still round-trip, which is why
        // selection is asserted in both directions below.
        let text = sample(&format!(
            r#"{{"linux-x64":[
                {{"url":"https://example.test/a.rpm","sha256":"{DIGEST}","installer":"rpm"}},
                {{"url":"https://example.test/a.deb","sha256":"{DIGEST}","installer":"deb"}}]}}"#
        ));
        let parsed = UpdateManifest::parse(&text).unwrap();
        // Preference is the caller's, because it knows what the host's
        // package manager is.
        let asset = parsed.preferred_asset("linux-x64", &["deb"]).unwrap();
        assert_eq!(asset.installer, "deb");
        // With no preference, the published order decides.
        let asset = parsed.preferred_asset("linux-x64", &[]).unwrap();
        assert_eq!(asset.installer, "rpm");
    }

    #[test]
    fn a_missing_platform_is_reported_as_no_asset() {
        let text = sample(r#"{"win-x64":[]}"#);
        let parsed = UpdateManifest::parse(&text).unwrap();
        let problem = parsed.assets_for("linux-arm64").unwrap_err();
        assert_eq!(problem.reason(), "no-asset");
        assert!(problem.detail().contains("linux-arm64"));
    }

    #[test]
    fn the_minimum_version_gates_an_in_place_install() {
        let text = sample(r#"{"win-x64":[]}"#);
        let parsed = UpdateManifest::parse(&text).unwrap();
        assert!(parsed.installable_from("0.1.9"));
        assert!(parsed.installable_from("0.2.0"));
        // An older build must not be replaced in place by an installer that
        // assumes a layout it does not have.
        assert!(!parsed.installable_from("0.1.5"));
    }

    #[test]
    fn a_missing_minimum_version_does_not_block_anything() {
        let text = r#"{"schema":1,"version":"0.2.0","platforms":{"win-x64":[]}}"#;
        let parsed = UpdateManifest::parse(text).unwrap();
        assert!(parsed.installable_from("0.1.0"));
    }
}
