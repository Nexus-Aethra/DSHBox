//! Version ordering for the updater.
//!
//! The one rule that matters here is that a version is a sequence of numbers,
//! not a string: `0.1.10` is *newer* than `0.1.9`, and comparing them as text
//! reads the `1` of `10` against the `2` of `9` and gets it backwards.
//!
//! The second rule is that a pre-release is *older* than the release it leads
//! to (`0.1.6-alpha.2 < 0.1.6`), which is the opposite of what deriving Ord
//! over a Vec of identifiers gives you, because an empty vector compares
//! less than a non-empty one. Both live in one place so the daemon, the CLI
//! and the desktop cannot each be subtly wrong in a different direction.

use std::cmp::Ordering;

/// A parsed version: numeric components plus optional pre-release identifiers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Version {
    numbers: Vec<u64>,
    pre: Vec<PreId>,
}

/// Ordering is deliberate: the variants are declared in precedence order so
/// the derived `Ord` puts a numeric identifier below an alphanumeric one,
/// which is what semver rule 11 requires. Reordering these variants would
/// silently invert pre-release precedence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PreId {
    /// Numeric identifiers order before alphanumeric ones (semver rule 11).
    Numeric(u64),
    Alpha(String),
}

impl Version {
    /// Parse `v0.1.9`, `0.1.9`, `0.1.9-alpha.2`, and a trailing `+build`
    /// (which is ignored for ordering).
    pub fn parse(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
        if trimmed.is_empty() {
            return None;
        }
        // Build metadata never participates in precedence.
        let core = trimmed.split('+').next().unwrap_or(trimmed);
        let (numbers, pre) = match core.split_once('-') {
            Some((numbers, pre)) => (numbers, Some(pre)),
            None => (core, None),
        };
        let numbers = numbers
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()?;
        if numbers.is_empty() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(text) => {
                // An empty pre-release (`0.1.9-`) is malformed, not equal to
                // the plain release: semver forbids an empty identifier here.
                if text.is_empty() {
                    return None;
                }
                let parts = text
                    .split('.')
                    .map(|part| match part.parse::<u64>() {
                        Ok(number) => PreId::Numeric(number),
                        Err(_) => PreId::Alpha(part.to_owned()),
                    })
                    .collect::<Vec<_>>();
                if parts.is_empty() {
                    return None;
                }
                parts
            }
        };
        Some(Self { numbers, pre })
    }

    /// Compare two versions the way a release list is ordered.
    pub fn compare(&self, other: &Self) -> Ordering {
        // Padding the shorter component list with zeros is what makes
        // `0.1` and `0.1.0` equal rather than "shorter sorts first".
        let width = self.numbers.len().max(other.numbers.len());
        for index in 0..width {
            let left = self.numbers.get(index).copied().unwrap_or(0);
            let right = other.numbers.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                non_equal => return non_equal,
            }
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            // No pre-release is newer than any pre-release of the same numbers.
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => self.pre.cmp(&other.pre),
        }
    }
}

/// Is `candidate` strictly newer than `current`?
///
/// Unparseable input is never "newer": a malformed remote tag must not look
/// like an upgrade, because acting on it would replace a working install with
/// whatever that text names.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(candidate), Some(current)) => candidate.compare(&current) == Ordering::Greater,
        _ => false,
    }
}

/// True when both names denote the same version, so a manifest that spells
/// `v0.1.9` matches a build that reports `0.1.9`.
pub fn same_version(left: &str, right: &str) -> bool {
    match (Version::parse(left), Version::parse(right)) {
        (Some(left), Some(right)) => left.compare(&right) == Ordering::Equal,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_digit_component_is_newer_than_one_digit() {
        // The reason this file exists: string ordering says otherwise.
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(!is_newer("0.1.9", "0.1.10"));
    }

    #[test]
    fn a_leading_v_and_build_metadata_do_not_change_the_answer() {
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(same_version("v0.1.9", "0.1.9"));
        assert!(same_version("0.1.9+build.7", "0.1.9"));
        assert!(!is_newer("0.1.9+v2", "0.1.9"));
    }

    #[test]
    fn a_prerelease_is_older_than_its_release() {
        assert!(is_newer("0.1.6", "0.1.6-alpha.2"));
        assert!(!is_newer("0.1.6-alpha.2", "0.1.6"));
        // ...and newer than the previous release it leads towards.
        assert!(is_newer("0.1.6-alpha.2", "0.1.5"));
    }

    #[test]
    fn prerelease_identifiers_follow_semver_precedence() {
        assert!(is_newer("0.1.6-alpha.10", "0.1.6-alpha.2"));
        assert!(is_newer("0.1.6-beta", "0.1.6-alpha"));
        // Numeric identifiers have *lower* precedence than alphanumeric ones
        // (semver rule 11), so `alpha.beta` is the newer of the two. Getting
        // this backwards is the classic pre-release mistake.
        assert!(is_newer("0.1.6-alpha.beta", "0.1.6-alpha.1"));
        assert!(!is_newer("0.1.6-alpha.1", "0.1.6-alpha.beta"));
    }

    #[test]
    fn a_missing_component_counts_as_zero() {
        assert!(same_version("0.1", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1"));
    }

    #[test]
    fn malformed_input_is_never_newer() {
        // A junk tag must not read as an upgrade.
        assert!(!is_newer("nightly", "0.1.9"));
        assert!(!is_newer("0.2.0", ""));
        assert!(!same_version("", "0.1.9"));
        assert!(Version::parse("..").is_none());
        assert!(Version::parse("0.1.9-").is_none());
    }
}
