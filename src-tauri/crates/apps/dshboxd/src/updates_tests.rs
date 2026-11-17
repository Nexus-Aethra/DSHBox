//! tests for updates.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    #[test]
    fn the_reported_version_is_the_application_version_not_the_crate_version() {
        // The bug this locks: dshboxd is an internal crate that keeps its own
        // 0.1.0, while the released version lives on the application package.
        // Reporting the crate version made a 0.1.9 install answer "0.1.0", so
        // is_newer called every published release newer and the pane offered
        // an "update" to the version already installed.
        assert_ne!(
            APP_VERSION,
            env!("CARGO_PKG_VERSION"),
            "the app version happens to equal the crate version, so this test cannot tell them apart"
        );
        assert!(
            !APP_VERSION.starts_with('v'),
            "the reported version must not carry a v prefix: {APP_VERSION}"
        );
        assert!(
            APP_VERSION.split('.').count() >= 3,
            "expected a semver-ish version, got {APP_VERSION}"
        );
    }

    #[test]
    fn a_release_at_the_current_version_is_not_an_update() {
        // The user-visible consequence of the bug above, stated directly.
        assert!(!is_newer(APP_VERSION, APP_VERSION));
        assert!(same_version(APP_VERSION, APP_VERSION));
    }

    #[test]
    fn the_throttle_spends_a_check_only_when_the_day_has_passed_or_never_ran() {
        assert!(should_check(&UpdateRecord::default(), "check"));
        let recent = UpdateRecord {
            checked_at: Some(now_seconds()),
            ..UpdateRecord::default()
        };
        assert!(!should_check(&recent, "check"));
        assert!(!should_check(&recent, "download"));
        // A stale record is due again.
        let stale = UpdateRecord {
            checked_at: Some(now_seconds() - CHECK_INTERVAL_SECONDS - 1),
            ..UpdateRecord::default()
        };
        assert!(should_check(&stale, "check"));
    }

    #[test]
    fn a_record_keeps_the_newest_release_so_up_to_date_is_distinguishable() {
        // The bug this locks: a successful check that found the installed
        // version used to clear `latest_version`, so the pane showed "no
        // release has been checked yet" right after saying it had checked.
        let record = UpdateRecord {
            checked_at: Some(now_seconds()),
            latest_version: Some(APP_VERSION.to_owned()),
            latest_tag: Some(format!("v{APP_VERSION}")),
            notes_url: None,
            last_error: None,
            ..UpdateRecord::default()
        };
        let payload = release_payload(&record, APP_VERSION);
        assert_eq!(
            payload["version"].as_str(),
            Some(APP_VERSION),
            "the newest release must still be reported"
        );
        assert_eq!(
            payload["newer"].as_bool(),
            Some(false),
            "installed is not an update"
        );
    }

    #[test]
    fn a_failed_check_reports_the_error_and_leaves_the_timestamp_alone() {
        // "last tried" and "last successful" are different facts; a failed
        // check must not advance the second one.
        let root = std::env::temp_dir().join(format!("dshbox-update-record-{}", std::process::id()));
        let _ = std::fs::create_dir_all(root.join("state"));
        let _ = std::fs::remove_file(record_path(&root));
        let seeded = UpdateRecord {
            checked_at: Some(12345),
            ..UpdateRecord::default()
        };
        write_record(&root, &seeded).unwrap();
        // Point the fetch at a host that cannot answer, then assert the
        // timestamp survived. The failure path is reached by an unreachable
        // mirror rather than by mocking the network.
        let error = perform_check(&root, Some("http://127.0.0.1:1"), None).unwrap_err();
        assert!(!error.is_empty());
        let after = read_record(&root);
        assert_eq!(after.checked_at, Some(12345), "a failure must not look like a success");
        assert_eq!(after.last_error.as_deref(), Some(error.as_str()));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn a_token_moves_the_manifest_off_the_mirror_too() {
        // The manifest carries the digests the mirror-served installer is
        // checked against; a mirror that served both would be the whole
        // trust chain. A token reads the manifest direct; without one the
        // mirror route stands.
        let url = manifest_url_for("v0.2.0", Some("https://gh-proxy.com"), Some("ghp_secret"));
        assert_eq!(url, latest_manifest_url("v0.2.0"));
        assert!(url.starts_with("https://github.com/"), "{url}");
        let mirrored = manifest_url_for("v0.2.0", Some("https://gh-proxy.com"), None);
        assert!(mirrored.starts_with("https://gh-proxy.com/"), "{mirrored}");
        // A blank token is no token.
        let mirrored = manifest_url_for("v0.2.0", Some("https://gh-proxy.com"), Some("  "));
        assert!(mirrored.starts_with("https://gh-proxy.com/"), "{mirrored}");
    }

    #[test]
    fn a_token_sends_the_api_direct_and_never_through_a_mirror() {
        // The rule this locks: a mirror is a third party, and an Authorization
        // header pointed at one hands the credential over. A token changes the
        // route, not just the headers.
        let (endpoint, authorization) =
            api_endpoint(LATEST_RELEASE_API, Some("https://gh-proxy.com"), Some("ghp_secret"));
        assert_eq!(endpoint, LATEST_RELEASE_API);
        assert_eq!(authorization.as_deref(), Some("Bearer ghp_secret"));
        // Padded tokens are a paste artifact, not a different credential.
        let (_, authorization) = api_endpoint(LATEST_RELEASE_API, None, Some("  ghp_secret  "));
        assert_eq!(authorization.as_deref(), Some("Bearer ghp_secret"));
    }

    #[test]
    fn without_a_token_the_mirror_route_survives() {
        let (endpoint, authorization) =
            api_endpoint(LATEST_RELEASE_API, Some("https://gh-proxy.com"), None);
        assert!(endpoint.starts_with("https://gh-proxy.com/"), "{endpoint}");
        assert_eq!(authorization, None);
        // A blank token is no token.
        let (endpoint, authorization) =
            api_endpoint(LATEST_RELEASE_API, Some("https://gh-proxy.com"), Some("   "));
        assert!(endpoint.starts_with("https://gh-proxy.com/"), "{endpoint}");
        assert_eq!(authorization, None);
    }

    #[test]
    fn a_newer_release_without_a_cached_answer_reads_as_unknown_not_installable() {
        // A record from an older daemon carries no asset; the payload must
        // say "not known" (manifest-unavailable) rather than present a
        // button for a file nobody has verified.
        let record = UpdateRecord {
            checked_at: Some(now_seconds()),
            latest_version: Some("9.0.0".to_owned()),
            latest_tag: Some("v9.0.0".to_owned()),
            ..UpdateRecord::default()
        };
        let payload = release_payload(&record, APP_VERSION);
        assert_eq!(payload["asset"], serde_json::Value::Null);
        assert_eq!(
            payload["manualReason"].as_str(),
            Some("manifest-unavailable")
        );
    }

    #[test]
    fn a_version_that_names_another_directory_is_refused() {
        assert!(clean_version("0.1.9").is_ok());
        assert!(clean_version("0.1.9-alpha.1").is_ok());
        assert!(clean_version("..").is_err());
        assert!(clean_version("0.1.9/..").is_err());
        assert!(clean_version("0.1.9\\..").is_err());
        assert!(clean_version(".").is_err());
        assert!(clean_version("").is_err());
        assert!(clean_version("0 1").is_err());
    }

    #[test]
    fn the_off_policy_never_checks_on_its_own() {
        // Off is a standing preference; an explicit check_update still runs,
        // because a person asking once outranks it.
        assert!(!should_check(&UpdateRecord::default(), "off"));
    }
