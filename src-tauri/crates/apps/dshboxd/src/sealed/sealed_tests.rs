//! tests for sealed.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
use super::pnpm_approval::{
    add_pnpm_build_approval, build_approval_hint, pnpm_git_allow_build_key, pnpm_ignored_build_keys,
};
    #[test]
    fn prepared_base_is_exposed_as_a_direct_runnable_template() {
        let temporary = tempfile::tempdir().unwrap();
        let base_directory = temporary.path().join("base");
        fs::create_dir_all(base_directory.join("harness")).unwrap();
        fs::write(base_directory.join("manifest.json"), "{}").unwrap();
        fs::write(base_directory.join("harness/package.json"), "{}").unwrap();
        let template = prepared_base_template_info(PreparedBaseRecord {
            id: "base-test".to_owned(),
            version: "dsh-v-test".to_owned(),
            source_ref: "github.com/deepseek-ai/deepseek-harness:dsh-v-test".to_owned(),
            source_commit: "test".to_owned(),
            directory: base_directory.to_string_lossy().into_owned(),
            created_at: 0,
            size_bytes: 0,
        })
        .expect("complete prepared base should be listed");
        assert_eq!(
            template.name,
            "github.com/deepseek-ai/deepseek-harness:dsh-v-test"
        );
        assert!(!template.built);
        assert_eq!(template.profile, "web");
    }

    #[test]
    fn incomplete_prepared_base_is_not_listed_as_runnable() {
        let temporary = tempfile::tempdir().unwrap();
        let template = prepared_base_template_info(PreparedBaseRecord {
            id: "base-test".to_owned(),
            version: "dsh-v-test".to_owned(),
            source_ref: "github.com/deepseek-ai/deepseek-harness:dsh-v-test".to_owned(),
            source_commit: "test".to_owned(),
            directory: temporary.path().to_string_lossy().into_owned(),
            created_at: 0,
            size_bytes: 0,
        });
        assert!(template.is_none());
    }

    #[test]
    fn transient_io_classifier_recognises_windows_file_lock_signatures() {
        let temporary = tempfile::tempdir().unwrap();
        let log_path = temporary.path().join("pnpm.log");

        let cases: &[(&str, bool)] = &[
            (
                "Progress: resolved 935, added 935, done\n[UNKNOWN] UNKNOWN: unknown error, open 'D:\\foo\\package.json'",
                true,
            ),
            (
                "ELSPackage pnpm ELSPackage error  EBUSY: resource busy or locked, open 'foo'",
                true,
            ),
            (
                "Error: EPERM: operation not permitted, open 'C:/foo/package.json'",
                true,
            ),
            (
                "EACCES: permission denied, scandir '/foo'",
                true,
            ),
            (
                "ELSPackage error EAGAIN: try again later",
                false,
            ),
            (
                "ELSPackage error EINVALIDTAG: Package name \"@scope/--\" is invalid",
                false,
            ),
            (
                "Lockfile is up to date, resolution step is skipped\nDone in 4s using pnpm",
                false,
            ),
        ];

        for (body, expected) in cases {
            fs::write(&log_path, body).unwrap();
            assert_eq!(
                log_indicates_transient_io(&log_path),
                *expected,
                "classifier returned wrong verdict for log body: {body}"
            );
        }
    }

    #[test]
    fn transient_io_classifier_handles_missing_log_file() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("does-not-exist.log");
        assert!(!log_indicates_transient_io(&missing));
    }

    fn write_manifest(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
    }

    fn read_manifest(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn add_pnpm_build_approval_merges_keys_idempotently_into_single_section() {
        let temporary = tempfile::tempdir().unwrap();
        let manifest = temporary.path().join("pnpm-workspace.yaml");

        // Case A: no allowBuilds section yet. First call creates the
        // section; second call must keep a single header and merge.
        write_manifest(&manifest, "packages:\n  - \"apps/*\"\n  - \"packages/*\"\n");
        add_pnpm_build_approval(&manifest, "node-pty@1.1.0").unwrap();
        add_pnpm_build_approval(&manifest, "sharp@0.33.5").unwrap();
        let body = read_manifest(&manifest);
        assert!(
            body.matches("allowBuilds:").count() == 1,
            "expected exactly one allowBuilds header, got: {body}"
        );
        // Keys are quoted: a scoped spec's leading `@` is a YAML reserved
        // indicator, so the bare scalar pnpm prints is not writable back.
        assert!(body.contains("  \"node-pty@1.1.0\": true"), "{body}");
        assert!(body.contains("  \"sharp@0.33.5\": true"), "{body}");
        assert!(
            body.contains("packages:\n  - \"apps/*\""),
            "preceding section must be preserved verbatim: {body}"
        );

        // Case B: pre-existing section with one quoted key (older Box
        // output). New bare-scalar key merges without duplicating the
        // header; existing quoted key survives intact.
        let manifest_b = temporary.path().join("pnpm-workspace-b.yaml");
        write_manifest(
            &manifest_b,
            "packages:\n  - \"apps/*\"\nallowBuilds:\n  \"left-pad@1.3.0\": true\n",
        );
        add_pnpm_build_approval(&manifest_b, "right-pad@2.0.0").unwrap();
        add_pnpm_build_approval(&manifest_b, "right-pad@2.0.0").unwrap(); // duplicate call
        let body_b = read_manifest(&manifest_b);
        assert_eq!(
            body_b.matches("allowBuilds:").count(),
            1,
            "section header must not duplicate: {body_b}"
        );
        assert_eq!(
            body_b.matches("  \"left-pad@1.3.0\": true").count(),
            1,
            "existing quoted key survives a rewrite (still present exactly once): {body_b}"
        );
        assert_eq!(
            body_b.matches("  \"right-pad@2.0.0\": true").count(),
            1,
            "new key appended exactly once even after duplicate call: {body_b}"
        );

        // Case C: keys are emitted in deterministic sorted order so the
        // diff against the original file is stable.
        let manifest_c = temporary.path().join("pnpm-workspace-c.yaml");
        write_manifest(&manifest_c, "allowBuilds:\n");
        add_pnpm_build_approval(&manifest_c, "zeta@1").unwrap();
        add_pnpm_build_approval(&manifest_c, "alpha@1").unwrap();
        add_pnpm_build_approval(&manifest_c, "mu@1").unwrap();
        let body_c = read_manifest(&manifest_c);
        let alpha_at = body_c.find("  \"alpha@1\": true").unwrap();
        let mu_at = body_c.find("  \"mu@1\": true").unwrap();
        let zeta_at = body_c.find("  \"zeta@1\": true").unwrap();
        assert!(
            alpha_at < mu_at && mu_at < zeta_at,
            "keys must be sorted: {body_c}"
        );
    }

    /// What shipped in v0.1.x: pnpm prints the key it wants as a bare scalar
    /// (`@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2`), and a leading `@` is a
    /// YAML reserved indicator — writing that back left the whole manifest
    /// unparseable, so the retry meant to approve the plugin died on
    /// `[ERROR] bad indentation of a mapping entry` instead of getting further.
    #[test]
    fn add_pnpm_build_approval_quotes_a_scoped_key_that_yaml_can_read_back() {
        let temporary = tempfile::tempdir().unwrap();
        let manifest = temporary.path().join("pnpm-workspace.yaml");
        // The exact header Box creates for a profile.
        write_manifest(
            &manifest,
            "packages:\n  - .\n\nnodeLinker: hoisted\nautoInstallPeers: false\n",
        );
        add_pnpm_build_approval(&manifest, "@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2").unwrap();
        let body = read_manifest(&manifest);
        assert!(
            body.contains("  \"@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2\": true"),
            "{body}"
        );
        assert!(
            !body.contains("\n  @"),
            "a scoped spec must never be written as a bare scalar: {body}"
        );
        let parsed: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
        assert_eq!(
            parsed["allowBuilds"]["@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2"],
            serde_yaml::Value::Bool(true)
        );
        assert_eq!(
            parsed["nodeLinker"],
            serde_yaml::Value::String("hoisted".to_owned())
        );
        assert_eq!(parsed["autoInstallPeers"], serde_yaml::Value::Bool(false));
    }

    #[test]
    fn add_pnpm_build_approval_keeps_the_lines_it_does_not_own() {
        let temporary = tempfile::tempdir().unwrap();
        let manifest = temporary.path().join("pnpm-workspace.yaml");
        // The real shape after a successful build: pnpm appended
        // minimumReleaseAgeExclude directly under the section without a blank
        // line, and a comment documents the approval. Neither is Box's to drop.
        write_manifest(
            &manifest,
            "allowBuilds:\n  # approved by the boxfile LABEL\n  \"node-pty@1.2.0-beta.15\": true\nminimumReleaseAgeExclude:\n  - '@nexus-aethra/dshell-std@0.1.14'\n",
        );
        add_pnpm_build_approval(&manifest, "@deepseek-ai/dsh-sandbox-local@0.2.1-rc.2").unwrap();
        let body = read_manifest(&manifest);
        assert!(
            body.contains("  # approved by the boxfile LABEL"),
            "comment line must survive: {body}"
        );
        assert_eq!(
            body.matches("allowBuilds:").count(),
            1,
            "section header must not duplicate: {body}"
        );
        let parsed: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
        assert_eq!(
            parsed["minimumReleaseAgeExclude"],
            serde_yaml::from_str::<serde_yaml::Value>("['@nexus-aethra/dshell-std@0.1.14']")
                .unwrap()
        );
        for key in [
            "node-pty@1.2.0-beta.15",
            "@deepseek-ai/dsh-sandbox-local@0.2.1-rc.2",
        ] {
            assert_eq!(
                parsed["allowBuilds"][key],
                serde_yaml::Value::Bool(true),
                "{body}"
            );
        }
    }

    #[test]
    fn build_approval_hint_repeats_neither_the_source_nor_the_key() {
        // A plugin's own build script is the common case: the ADD spec and the
        // key pnpm asks for are the same string, and listing it twice made the
        // suggestion look unlike the line the user had already written, so they
        // pasted it again and the build failed again.
        let hint = build_approval_hint(
            "@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2",
            Some("@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2"),
        );
        assert_eq!(
            hint.matches("@deepseek-ai/dsh-subprocess-local@0.2.1-rc.2")
                .count(),
            1,
            "{hint}"
        );
        let both = build_approval_hint(
            "@deepseek-ai/dsh-ssh@0.2.1-rc.2",
            Some("node-pty@1.2.0-beta.15"),
        );
        assert!(
            both.contains(
                "LABEL dshbox.allow-build=@deepseek-ai/dsh-ssh@0.2.1-rc.2,node-pty@1.2.0-beta.15"
            ),
            "{both}"
        );
    }

    #[test]
    fn pnpm_git_allow_build_key_extracts_pnpm_printable_key() {
        let temporary = tempfile::tempdir().unwrap();
        let log_path = temporary.path().join("pnpm.log");

        // Real log captured from `pnpm dsh plugin add` on
        // dsh-better-sidebar@v0.12.3.
        let body = "[ERR_PNPM_GIT_DEP_PREPARE_NOT_ALLOWED] Failed to prepare git-hosted package fetched from \"https://codeload.github.com/omdsh-dev/DSH-better-sidebar/tar.gz/ab785d71fb194ea9db4487461af8f5d40e67c216\": The git-hosted package \"dsh-better-sidebar@0.12.3\" needs to execute build scripts but is not in the \"allowBuilds\" allowlist.\n\nThis error happened while installing a direct dependency of /home/wpp/tmp\n\nAdd the package to \"allowBuilds\" in your project's pnpm-workspace.yaml to allow it to run scripts. For example:\nallowBuilds:\n  dsh-better-sidebar@https://codeload.github.com/omdsh-dev/DSH-better-sidebar/tar.gz/ab785d71fb194ea9db4487461af8f5d40e67c216: true\n";
        fs::write(&log_path, body).unwrap();
        assert_eq!(
            pnpm_git_allow_build_key(&log_path).as_deref(),
            Some(
                "dsh-better-sidebar@https://codeload.github.com/omdsh-dev/DSH-better-sidebar/tar.gz/ab785d71fb194ea9db4487461af8f5d40e67c216"
            ),
        );

        // No marker returns None.
        let log_b = temporary.path().join("plain.log");
        fs::write(&log_b, "Done in 12s using pnpm v11.7.0\n").unwrap();
        assert!(pnpm_git_allow_build_key(&log_b).is_none());

        // Missing file returns None without panicking.
        let missing = temporary.path().join("missing.log");
        assert!(pnpm_git_allow_build_key(&missing).is_none());
    }

    #[test]
    fn pnpm_ignored_build_keys_extracts_keys_through_prefixed_log_lines() {
        let temporary = tempfile::tempdir().unwrap();
        let log_path = temporary.path().join("pnpm.log");

        let cases: &[(&str, &[&str])] = &[
            // The real shape from `pnpm install` after a plugin add —
            // a `...hash pnpm-install:` prefix sits before the marker.
            (
                "...5639_adc99faf781f12cc1eb3f132dd14e2a5 pnpm-install: [ERR_PNPM_IGNORED_BUILDS] Ignored build scripts: node-pty@1.1.0\n",
                &["node-pty@1.1.0"],
            ),
            // Multi-key form on a single line, comma-separated.
            (
                "[ERR_PNPM_IGNORED_BUILDS] Ignored build scripts: sharp@0.33.5, esbuild@0.21.5\n",
                &["sharp@0.33.5", "esbuild@0.21.5"],
            ),
            // No marker at all.
            (
                "Progress: resolved 341, added 341, done\nDone in 12s\n",
                &[],
            ),
        ];

        for (body, expected) in cases {
            fs::write(&log_path, body).unwrap();
            let actual: Vec<String> = pnpm_ignored_build_keys(&log_path);
            let expected: Vec<String> = expected.iter().map(|key| (*key).to_owned()).collect();
            assert_eq!(
                actual, expected,
                "pnpm_ignored_build_keys returned wrong keys for log body: {body}"
            );
        }

        // Missing file yields an empty vector without panicking.
        let missing = temporary.path().join("missing.log");
        assert!(pnpm_ignored_build_keys(&missing).is_empty());
    }
