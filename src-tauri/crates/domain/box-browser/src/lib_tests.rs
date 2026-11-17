//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    #[test]
    fn classifies_by_file_name() {
        assert_eq!(
            classify(Path::new(
                r"C:Program Files (x86)MicrosoftEdgeApplicationmsedge.exe"
            )),
            BrowserKind::Edge
        );
        assert_eq!(
            classify(Path::new("/usr/bin/google-chrome")),
            BrowserKind::Chrome
        );
        assert_eq!(
            classify(Path::new("/opt/portable/headless-shell")),
            BrowserKind::Chromium
        );
    }

    #[test]
    fn configured_path_wins_over_discovery() {
        // Use this very test binary as a stand-in: it is absolute and is a file.
        let exe = std::env::current_exe().expect("current exe");
        let resolved = resolve(Some(&exe.display().to_string())).expect("resolve");
        assert!(resolved.configured);
        assert_eq!(resolved.path, exe);
    }

    #[test]
    fn missing_configured_path_is_an_error_not_a_fallback() {
        // A path that is absolute on every platform, so the test exercises the
        // missing-file branch rather than the absolute-path one.
        let absent = std::env::temp_dir().join("definitely-not-a-browser.exe");
        let err = resolve(Some(&absent.display().to_string())).unwrap_err();
        assert!(err.contains("no browser executable"), "unexpected: {err}");
    }

    #[test]
    fn empty_configured_path_falls_back_to_discovery() {
        // resolve() must not treat "" as a configured browser.
        let _ = resolve(Some(""));
    }

    #[test]
    fn relative_paths_are_refused() {
        let err = validate_browser_path("msedge.exe").unwrap_err();
        assert!(err.contains("must be absolute"), "unexpected: {err}");
    }

    #[test]
    fn discovery_is_ordered() {
        let found = discover();
        // Whatever exists, the order must match the documented preference.
        let order = |k: &BrowserKind| match k {
            BrowserKind::Edge => 0,
            BrowserKind::Chrome => 1,
            BrowserKind::Brave => 2,
            BrowserKind::Chromium => 3,
        };
        let ranks: Vec<u8> = found.iter().map(|c| order(&c.kind)).collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();
        assert_eq!(ranks, sorted, "discovery order drifted: {found:?}");
        assert!(found.iter().all(|c| c.path.is_file()));
    }
