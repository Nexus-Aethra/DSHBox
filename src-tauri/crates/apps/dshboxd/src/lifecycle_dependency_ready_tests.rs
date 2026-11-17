//! dependency_ready_tests for lifecycle.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::fs;

    #[test]
    fn declared_env_reads_string_entries_and_ignores_the_rest() {
        let value = serde_json::json!({
            "id": "container-1",
            "env": {
                "OPENAI_API_KEY": "sk-test",
                "TZ": "Asia/Shanghai",
                "PORT": 8080,
            },
        });
        let declared = declared_env_from_metadata(&value);
        assert_eq!(
            declared.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert_eq!(
            declared.get("TZ").map(String::as_str),
            Some("Asia/Shanghai")
        );
        assert!(
            !declared.contains_key("PORT"),
            "non-string entry kept: {declared:?}"
        );
        // A container written before ENV existed simply declares nothing.
        let legacy = serde_json::json!({"id": "container-1"});
        assert!(declared_env_from_metadata(&legacy).is_empty());
    }

    #[test]
    fn parses_the_announced_authenticated_url_from_the_log_tail() {
        let log = "\
dsh web: http://127.0.0.1:40413/?token=3xqhaJFec8uZFUxRsyzL0lXRqfVxpi-li-F93sBnqdE
dsh web: opening the default browser; pass --no-open to disable
";
        assert_eq!(
            parse_announced_host_url(log),
            Some(
                "http://127.0.0.1:40413/?token=3xqhaJFec8uZFUxRsyzL0lXRqfVxpi-li-F93sBnqdE"
                    .to_owned()
            )
        );
    }

    #[test]
    fn parse_takes_the_url_token_and_ignores_the_lan_suffix() {
        let log =
            "dsh web: http://127.0.0.1:3080/?token=abc (LAN: http://192.168.1.4:3080/?token=abc)\n";
        assert_eq!(
            parse_announced_host_url(log),
            Some("http://127.0.0.1:3080/?token=abc".to_owned())
        );
    }

    #[test]
    fn parse_ignores_noise_and_non_loopback_lines() {
        assert_eq!(parse_announced_host_url(""), None);
        assert_eq!(parse_announced_host_url("worker started\n"), None);
        assert_eq!(
            parse_announced_host_url("dsh web: http://192.168.1.4:3080/?token=abc\n"),
            None
        );
    }

    #[test]
    fn requires_the_tsx_manifest_not_just_node_modules() {
        let root = std::env::temp_dir().join(format!(
            "dshbox-dependency-ready-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(root.join("node_modules")).unwrap();
        assert!(!dsh_dependencies_ready(&root));

        fs::create_dir_all(root.join("node_modules/tsx")).unwrap();
        fs::write(root.join("node_modules/tsx/package.json"), "{}").unwrap();
        assert!(dsh_dependencies_ready(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn recognises_retryable_loopback_bind_errors() {
        assert!(is_transient_loopback_bind_failure(
            b"Error: listen EACCES: permission denied 127.0.0.1:54329"
        ));
        assert!(is_transient_loopback_bind_failure(
            b"Error: listen EADDRINUSE: address already in use 127.0.0.1:54329"
        ));
        assert!(!is_transient_loopback_bind_failure(
            b"plugin tree failed to load"
        ));
    }
