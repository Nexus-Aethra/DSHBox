//! tests for updates.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    #[test]
    fn the_windows_helper_waits_for_this_process_then_installs_and_relaunches() {
        let script = render_installer_script(
            InstallerPlatform::WindowsMsi,
            4242,
            r#"C:\Program Files\DSH Box\dshbox.exe"#,
            r#"D:\dshbox-data\updates\0.2.0\dshbox.msi"#,
            r#"D:\dshbox-data\updates\0.2.0"#,
        );
        // Waiting on the process id, not a fixed sleep: a fixed wait either
        // fails the install on a locked file or wastes time.
        assert!(script.contains(r#"tasklist /FI "PID eq 4242""#), "{script}");
        assert!(script.contains("goto wait_app"), "{script}");
        // `timeout` refuses to wait with redirected stdin; `ping` does.
        assert!(script.contains("ping -n 2 127.0.0.1 >nul"), "{script}");
        assert!(script.contains("ping -n 3 127.0.0.1 >nul"), "{script}");
        assert!(!script.contains("timeout /t"), "{script}");
        // The daemon holds its own executable open and would fail the MSI.
        assert!(script.contains("taskkill /F /IM dshboxd.exe"), "{script}");
        assert!(
            script.contains(r#"msiexec /i "D:\dshbox-data\updates\0.2.0\dshbox.msi" /qn /norestart"#),
            "{script}"
        );
        assert!(
            script.contains(r#"start "" "C:\Program Files\DSH Box\dshbox.exe""#),
            "{script}"
        );
        // 3010 is success-with-reboot, not a failure.
        assert!(script.contains(r#"if not "%RESULT%"=="3010""#), "{script}");
    }

    #[test]
    fn the_macos_helper_replaces_the_bundle_beside_the_running_one() {
        let script = render_installer_script(
            InstallerPlatform::MacDmg,
            4242,
            "/Applications/DSH Box.app/Contents/MacOS/dshbox",
            "/tmp/dshbox_0.2.0_aarch64.dmg",
            "/tmp",
        );
        assert!(script.contains("while kill -0 4242"), "{script}");
        assert!(script.contains("hdiutil attach"), "{script}");
        assert!(script.contains("hdiutil detach"), "{script}");
        // The destination is the directory CONTAINING the bundle: copying
        // into Contents/MacOS nests the new bundle inside the old one and
        // relaunches the old binary -- an update that reports success and
        // updates nothing.
        assert!(script.contains("destination=\"/Applications\""), "{script}");
        assert!(!script.contains("destination=\"/Applications/DSH Box.app\""), "{script}");
        assert!(script.contains("cp -R"), "{script}");
    }

    #[test]
    fn the_bundle_destination_is_the_app_bundle_parent() {
        assert_eq!(
            bundle_install_destination("/Applications/DSH Box.app/Contents/MacOS/dshbox")
                .as_deref(),
            Some("/Applications")
        );
        // A launcher with no .app ancestor has no bundle to walk out of.
        assert_eq!(bundle_install_destination("/usr/local/bin/dshbox"), None);
    }

    #[test]
    fn windows_paths_are_quoted_for_cmd() {
        // Quotes double inside a quoted argument; a literal % must double or
        // cmd expands it as a variable.
        assert_eq!(cmd_quote("C:\\Program Files\\a\"b"), "C:\\Program Files\\a\"\"b");
        assert_eq!(cmd_quote("D:\\x%y\\z.msi"), "D:\\x%%y\\z.msi");
    }

    #[test]
    fn a_missing_installer_path_is_not_a_silent_success() {
        // The failure mode this rules out: an update that reports "scheduled"
        // and then does nothing because the file it names is gone.
        let state = serde_json::json!({
            "policy": "auto",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "msi" } },
            "download": null,
        });
        assert!(install_on_quit_candidate(&state).is_none());
    }

    #[test]
    fn install_on_quit_needs_the_auto_policy() {
        let state = serde_json::json!({
            "policy": "download",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "msi" } },
            "download": { "path": "C:\\x\\dshbox.msi" },
        });
        assert!(install_on_quit_candidate(&state).is_none());
    }

    #[test]
    fn install_on_quit_is_refused_where_no_unattended_installer_exists() {
        let state = serde_json::json!({
            "policy": "auto",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "deb" } },
            "download": { "path": "/tmp/dshbox.deb" },
        });
        assert!(install_on_quit_candidate(&state).is_none());
    }

    #[test]
    fn install_on_quit_accepts_a_ready_automatic_update() {
        let state = serde_json::json!({
            "policy": "auto",
            "available": { "newer": true, "version": "0.2.0", "asset": { "installer": "msi" } },
            "download": { "path": "C:\\x\\dshbox.msi" },
        });
        let candidate = install_on_quit_candidate(&state).expect("a candidate");
        assert_eq!(candidate["version"], "0.2.0");
    }

    #[test]
    fn linux_installs_are_refused_rather_than_faked() {
        // A deb needs root; the pane shows the command instead of pretending.
        assert!(installer_platform("deb").is_none());
        assert!(installer_platform("rpm").is_none());
        assert!(installer_platform("msi").is_some());
        assert!(installer_platform("dmg").is_some());
    }
