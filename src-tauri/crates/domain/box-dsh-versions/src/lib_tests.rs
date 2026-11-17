//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use box_foundation::now_seconds;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dshbox-versions-{}-{}-{name}",
            std::process::id(),
            now_seconds()
        ));
        let _ = fs::remove_dir_all(&dir);
        // A finished install carries the completion marker written by
        // `pull_template`; `.git` alone means a clone still in flight.
        let source = dir.join("runtimes/v0.1.0/source");
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::write(
            source.join(".dshbox-runtime.json"),
            "{\"version\":\"v0.1.0\",\"commit\":\"deadbeef\"}",
        )
        .unwrap();
        dir
    }

    fn catalog(names: &[&str]) -> Vec<String> {
        let mut versions: Vec<DshVersion> = names
            .iter()
            .map(|name| DshVersion { name: (*name).to_owned(), installed: false })
            .collect();
        sort_catalog(&mut versions);
        versions.into_iter().map(|version| version.name).collect()
    }

    #[test]
    fn the_catalog_reads_newest_first_with_latest_on_top() {
        // The Harness tab listed `dsh-v0.1.0-rc.7` first and `latest` last, because
        // the entries were collected into a `BTreeMap` and handed out in the byte
        // order of their names — the oldest release at the top, the moving
        // reference the others are cut from at the bottom.
        assert_eq!(
            catalog(&["dsh-v0.1.0-rc.7", "latest", "dsh-v0.1.6-alpha.2", "dsh-v0.1.5-rc.2"]),
            vec!["latest", "dsh-v0.1.6-alpha.2", "dsh-v0.1.5-rc.2", "dsh-v0.1.0-rc.7"]
        );
    }

    #[test]
    fn a_two_digit_number_is_not_read_as_a_string() {
        // Byte order compares the `1` of `10` against the `2` of `2` and stops, so
        // 0.1.10 was going to sort above 0.1.2 the first time DSH shipped one.
        assert_eq!(
            catalog(&["dsh-v0.1.2-rc.1", "dsh-v0.1.10-alpha.1", "dsh-v0.1.9"]),
            vec!["dsh-v0.1.10-alpha.1", "dsh-v0.1.9", "dsh-v0.1.2-rc.1"]
        );
    }

    #[test]
    fn a_release_outranks_its_own_pre_releases() {
        // `0.1.6` is the release `0.1.6-alpha.2` was cut from. Deriving the order
        // would have said the opposite: an empty identifier list compares less than
        // a non-empty one.
        assert_eq!(
            catalog(&["dsh-v0.1.6-alpha.2", "dsh-v0.1.6", "dsh-v0.1.6-rc.1"]),
            vec!["dsh-v0.1.6", "dsh-v0.1.6-rc.1", "dsh-v0.1.6-alpha.2"]
        );
    }

    #[test]
    fn an_unreadable_name_is_still_listed() {
        // A tag that is neither `latest` nor a version keeps a place, at the end
        // and in name order, rather than being dropped from the list.
        assert_eq!(
            catalog(&["zzz-custom", "dsh-v0.1.0", "aaa-custom", "latest"]),
            vec!["latest", "dsh-v0.1.0", "aaa-custom", "zzz-custom"]
        );
    }

    #[test]
    fn in_flight_clone_is_not_reported_installed() {
        let dir = fixture("in-flight");
        // Simulate a harness clone that started (`.git` created) but has
        // not finished (no marker yet): it must not count as installed,
        // otherwise the UI shows "installed" mid-download.
        let source = dir.join("runtimes/latest/source");
        fs::create_dir_all(source.join(".git")).unwrap();
        let versions = installed_versions(dir.to_str().unwrap()).unwrap();
        assert_eq!(versions, vec!["v0.1.0"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_template_ref_defaults_to_latest() {
        let parsed = parse_template_ref("github.com/deepseek-ai/deepseek-harness").unwrap();
        assert_eq!(parsed.url, "https://github.com/deepseek-ai/deepseek-harness");
        assert_eq!(parsed.tag, None);
        assert_eq!(parsed.version, "latest");
    }

    #[test]
    fn sanitize_template_ref_keeps_url_separators() {
        // With the hash-addressed layout the template body lives under
        // `<hash>/script.dsh`; `_sanitize_template_ref` is only kept for
        // the legacy flat-file alias so it must keep URL separators around
        // (only `\` is folded) to read more naturally.
        assert_eq!(
            sanitize_template_ref("github.com/deepseek-ai/deepseek-harness:latest"),
            "github.com/deepseek-ai/deepseek-harness:latest"
        );
        assert_eq!(
            sanitize_template_ref("  github.com/foo/bar  "),
            "github.com/foo/bar"
        );
    }

    #[test]
    fn parse_template_ref_latest_is_preserved_for_display() {
        // The parse layer keeps `latest` as the version tag so the templates
        // list and version picker show it; `pull_template` separately maps
        // `latest` to the repository HEAD before cloning.
        let parsed = parse_template_ref("github.com/deepseek-ai/deepseek-harness:latest").unwrap();
        assert_eq!(parsed.url, "https://github.com/deepseek-ai/deepseek-harness");
        assert_eq!(parsed.tag.as_deref(), Some("latest"));
        assert_eq!(parsed.version, "latest");
    }

    #[test]
    fn parse_template_ref_accepts_colon_tag() {
        let parsed =
            parse_template_ref("github.com/deepseek-ai/deepseek-harness:v0.1.0").unwrap();
        assert_eq!(parsed.tag.as_deref(), Some("v0.1.0"));
        assert_eq!(parsed.version, "v0.1.0");
    }

    #[test]
    fn parse_template_ref_accepts_at_ref() {
        let parsed =
            parse_template_ref("github.com/deepseek-ai/deepseek-harness@v0.1.0").unwrap();
        assert_eq!(parsed.tag.as_deref(), Some("v0.1.0"));
        assert_eq!(parsed.version, "v0.1.0");
    }

    #[test]
    fn parse_template_ref_rejects_unknown_hosts() {
        assert!(parse_template_ref("example.com/foo/bar").is_err());
    }
