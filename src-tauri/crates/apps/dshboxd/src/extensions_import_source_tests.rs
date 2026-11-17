//! import_source_tests for extensions.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    #[test]
    fn a_package_spec_becomes_a_reference() {
        match classify_import_source("@nexus-aethra/dshell-ssh@0.1.5").unwrap() {
            ImportSource::Reference { name, version, spec } => {
                assert_eq!(name, "@nexus-aethra/dshell-ssh");
                assert_eq!(version.as_deref(), Some("0.1.5"));
                assert_eq!(spec, "@nexus-aethra/dshell-ssh@0.1.5");
            }
            _ => panic!("a package spec must not be copied"),
        }
        match classify_import_source("npm:zod@3.22.0").unwrap() {
            ImportSource::Reference { name, version, .. } => {
                assert_eq!(name, "zod");
                assert_eq!(version.as_deref(), Some("3.22.0"));
            }
            _ => panic!("npm: prefix must resolve to a reference"),
        }
        // A bare name has no version to pin; it still points at the registry.
        match classify_import_source("cordis-plugin-bar").unwrap() {
            ImportSource::Reference { version, .. } => assert!(version.is_none()),
            _ => panic!("a bare name must be a reference"),
        }
    }

    #[test]
    fn a_local_directory_is_copied() {
        let directory = std::env::temp_dir().join(format!("box-ext-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        match classify_import_source(directory.to_string_lossy().as_ref()).unwrap() {
            ImportSource::Local(path) => assert_eq!(path, directory),
            _ => panic!("a local directory must be copied in"),
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_remote_archive_points_at_the_boxfile_instead() {
        let error = match classify_import_source("https://example.com/plugin.tar.gz") {
            Ok(_) => panic!("a remote archive must not be silently accepted"),
            Err(error) => error,
        };
        assert!(error.contains("boxfile"), "{error}");
    }
