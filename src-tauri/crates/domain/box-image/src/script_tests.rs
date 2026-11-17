//! tests for script.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    fn base_dir() -> PathBuf {
        PathBuf::from("/tmp")
    }

    #[test]
    fn parse_minimal_script() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(
            script.harness_url,
            "https://github.com/deepseek-ai/deepseek-harness"
        );
        assert_eq!(script.harness_ref.as_deref(), Some("latest"));
        assert_eq!(script.base_template, None);
        assert_eq!(script.profile, "web");
        assert_eq!(script.name, "image");
        assert_eq!(script.version, "latest");
        assert!(script.ops.is_empty());
    }

    #[test]
    fn parse_from_local_template_name() {
        let script = parse_script(
            "FROM web-base\nPROFILE web\nADD plugin cordis-plugin-foo\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.base_template.as_deref(), Some("web-base"));
        assert_eq!(script.harness_url, "");
        assert_eq!(script.harness_ref, None);
        assert_eq!(script.ops.len(), 1);
        // Scoped names are never mistaken for template references.
        let script = parse_script("FROM @scope/base\nPROFILE web\n", &base_dir()).unwrap();
        assert_eq!(script.base_template.as_deref(), Some("@scope/base"));
    }

    #[test]
    fn parse_from_missing_is_an_error() {
        assert!(parse_script("PROFILE web\n", &base_dir()).is_err());
    }

    #[test]
    fn parse_full_script() {
        let script = parse_script(
            "# a comment\n\
             FROM github.com/deepseek-ai/deepseek-harness:latest\n\
             PROFILE web\n\
             NAME team-stack\n\
             VERSION 2.1.0\n\
             LABEL maintainer=alice@example.com\n\
             ADD plugin github.com/team/cordis-plugin-foo:1.2.3\n\
             ADD plugin https://intranet/foo.tgz\n\
             ADD plugin ./plugins/secret\n\
             ADD plugin cordis-plugin-bar@1.0.0\n\
             ADD skill team-conventions\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.name, "team-stack");
        assert_eq!(script.version, "2.1.0");
        assert_eq!(
            script.labels.get("maintainer"),
            Some(&"alice@example.com".to_string())
        );
        assert_eq!(script.ops.len(), 5);
        match &script.ops[0].op_for_test() {
            ParsedSource::Github { url, ref_ } => {
                assert_eq!(url, "https://github.com/team/cordis-plugin-foo");
                assert_eq!(ref_.as_deref(), Some("1.2.3"));
            }
            _ => panic!("expected github source"),
        }
        match &script.ops[1].op_for_test() {
            ParsedSource::Tarball { url, local } => {
                assert_eq!(url, "https://intranet/foo.tgz");
                assert!(!local);
            }
            _ => panic!("expected tarball source"),
        }
        match &script.ops[2].op_for_test() {
            ParsedSource::LocalDir { path } => {
                assert!(path.ends_with("plugins/secret"));
            }
            _ => panic!("expected local dir source"),
        }
        match &script.ops[3].op_for_test() {
            ParsedSource::BareName {
                name,
                scope,
                version,
            } => {
                assert_eq!(name, "cordis-plugin-bar");
                assert!(scope.is_none());
                assert_eq!(version.as_deref(), Some("1.0.0"));
            }
            _ => panic!("expected bare name source"),
        }
        let ImageOp::Add { kind, .. } = &script.ops[4];
        assert!(matches!(kind, AddKind::Skill));
    }

    #[test]
    fn from_github_short_form_keeps_latest_tag() {
        // `:latest` is preserved in the parsed ref so templates list and
        // version picker can show the source; the clone path interprets it
        // as the repository HEAD separately.
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest
PROFILE web
",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(
            script.harness_url,
            "https://github.com/deepseek-ai/deepseek-harness"
        );
        assert_eq!(script.harness_ref.as_deref(), Some("latest"));
    }

    #[test]
    fn from_github_short_form_with_at_ref_extracts_ref() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness@v0.1.0
PROFILE web
",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.harness_ref.as_deref(), Some("v0.1.0"));
    }

    #[test]
    fn from_literal_value_names_a_local_template() {
        // A non-GitHub FROM value is a local template name (e.g. the base
        // template for a version); the builder resolves it under
        // `<root>/templates/`.
        let script = parse_script(
            "FROM v0.2.0
PROFILE web
",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.base_template.as_deref(), Some("v0.2.0"));
        assert_eq!(script.harness_url, "");
        assert!(script.harness_ref.is_none());
    }

    #[test]
    fn harness_digest_is_stable() {
        let a = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        let b = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE headless\n",
            &base_dir(),
        )
        .unwrap();
        // Same FROM value yields the same digest regardless of the rest of
        // the script (the digest is purely a function of the URL + ref).
        assert_eq!(a.harness_digest(), b.harness_digest());
    }

    #[test]
    fn harness_digest_differs_on_ref() {
        let a = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        let b = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:v0.1.0\nPROFILE web\n",
            &base_dir(),
        )
        .unwrap();
        assert_ne!(a.harness_digest(), b.harness_digest());
    }

    #[test]
    fn missing_from_is_reported() {
        let error = parse_script("PROFILE web\n", &base_dir()).unwrap_err();
        match error {
            ImageError::MissingDirective { name, .. } => assert_eq!(name, "FROM"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn github_short_form_with_at_ref() {
        let source = parse_source_token(1, "github.com/team/repo@v1.0.0", &base_dir()).unwrap();
        match source {
            ParsedSource::Github { url, ref_ } => {
                assert_eq!(url, "https://github.com/team/repo");
                assert_eq!(ref_.as_deref(), Some("v1.0.0"));
            }
            _ => panic!("expected github source"),
        }
    }

    #[test]
    fn relative_path_becomes_local_dir() {
        let source = parse_source_token(2, "./plugins/secret", &PathBuf::from("/work")).unwrap();
        match source {
            ParsedSource::LocalDir { path } => {
                assert!(path.ends_with("plugins/secret"));
            }
            _ => panic!("expected local dir"),
        }
    }

    #[test]
    fn bare_name_with_scope_and_version() {
        let source = parse_source_token(3, "@scope/cordis-plugin-bar:1.2.3", &base_dir()).unwrap();
        match source {
            ParsedSource::BareName {
                name,
                scope,
                version,
            } => {
                assert_eq!(scope.as_deref(), Some("scope"));
                assert_eq!(name, "cordis-plugin-bar");
                assert_eq!(version.as_deref(), Some("1.2.3"));
            }
            _ => panic!("expected bare name"),
        }
    }

    #[test]
    fn add_data_kind_is_accepted() {
        // `ADD data` marks a non-code payload: the parser must accept it and
        // carry the kind through to the manifest as `resourceType: data`.
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\nADD data thing\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.ops.len(), 1);
        let ImageOp::Add { kind, source, .. } = &script.ops[0];
        assert!(matches!(kind, AddKind::Data));
        match source {
            ParsedSource::BareName { name, .. } => assert_eq!(name, "thing"),
            _ => panic!("expected bare name source"),
        }
    }

    #[test]
    fn cp_alias_is_accepted_and_defaults_to_plugin() {
        // `CP` is the legacy alias for `ADD plugin`. Old scripts with a bare
        // source token continue to parse and get the Plugin kind.
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\nCP cordis-plugin-foo\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.ops.len(), 1);
        let ImageOp::Add { kind, source, .. } = &script.ops[0];
        assert!(matches!(kind, AddKind::Plugin));
        match source {
            ParsedSource::BareName { name, .. } => assert_eq!(name, "cordis-plugin-foo"),
            _ => panic!("expected bare name source"),
        }
    }

    #[test]
    fn parse_env_directives() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\n\
             PROFILE web\n\
             ENV OPENAI_API_KEY=sk-test\n\
             ENV TZ=Asia/Shanghai\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(
            script.env.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-test")
        );
        assert_eq!(
            script.env.get("TZ").map(String::as_str),
            Some("Asia/Shanghai")
        );
    }

    #[test]
    fn parse_env_last_declaration_wins() {
        let script = parse_script(
            "FROM github.com/deepseek-ai/deepseek-harness:latest\n\
             PROFILE web\n\
             ENV TZ=UTC\n\
             ENV TZ=Asia/Shanghai\n",
            &base_dir(),
        )
        .unwrap();
        assert_eq!(script.env.len(), 1);
        assert_eq!(
            script.env.get("TZ").map(String::as_str),
            Some("Asia/Shanghai")
        );
    }

    #[test]
    fn parse_env_rejects_malformed_lines() {
        let from_profile = "FROM github.com/deepseek-ai/deepseek-harness:latest\nPROFILE web\n";
        // No `=` at all.
        let error =
            parse_script(&format!("{from_profile}ENV OPENAI_API_KEY\n"), &base_dir()).unwrap_err();
        assert!(matches!(error, ImageError::Syntax { .. }), "{error:?}");
        // Not a variable name.
        let error = parse_script(&format!("{from_profile}ENV 1BAD=x\n"), &base_dir()).unwrap_err();
        assert!(matches!(error, ImageError::Syntax { .. }), "{error:?}");
        // Two pairs on one line.
        let error = parse_script(&format!("{from_profile}ENV A=1 B=2\n"), &base_dir()).unwrap_err();
        assert!(matches!(error, ImageError::Syntax { .. }), "{error:?}");
    }

    // Test-only accessor for the variant's source. Avoids a `pub` field on
    // the production enum (we keep its variants private to the parser).
    impl ImageOp {
        pub(crate) fn op_for_test(&self) -> &ParsedSource {
            match self {
                ImageOp::Add { source, .. } => source,
            }
        }
    }
