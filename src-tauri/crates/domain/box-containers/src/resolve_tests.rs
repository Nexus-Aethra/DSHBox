//! tests for resolve.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::fs;

    /// A container as the scanner sees it: an `instances/<id>` directory whose
    /// `container.json` carries the identity.
    fn make_container(root: &Path, id: &str, name: &str) {
        let directory = root.join("instances").join(id);
        fs::create_dir_all(directory.join("state")).unwrap();
        fs::write(
            directory.join("container.json"),
            serde_json::json!({
                "id": id,
                "name": name,
                "version": "latest",
                "profile": "web",
            })
            .to_string(),
        )
        .unwrap();
    }

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dshbox-resolve-{label}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_full_id_resolves_as_it_always_did() {
        let root = scratch("full-id");
        make_container(&root, "container-1730000000", "alpha");
        let found = resolve_container_in_root(&root, "container-1730000000").unwrap();
        assert_eq!(found.id, "container-1730000000");
        assert!(found.directory.is_dir());
    }

    #[test]
    fn a_name_resolves_to_the_same_container() {
        let root = scratch("name");
        make_container(&root, "container-1730000001", "alpha");
        let found = resolve_container_in_root(&root, "alpha").unwrap();
        assert_eq!(found.id, "container-1730000001");
        // The list-based API the CLI shares answers the same way.
        let listed = vec![crate::scan_containers(&root.to_string_lossy())
            .unwrap()
            .remove("container-1730000001")
            .unwrap()];
        assert_eq!(match_container(&listed, "alpha").unwrap().id, "container-1730000001");
    }

    #[test]
    fn the_bare_timestamp_part_resolves() {
        let root = scratch("bare");
        make_container(&root, "container-1730000002", "alpha");
        let found = resolve_container_in_root(&root, "1730000002").unwrap();
        assert_eq!(found.id, "container-1730000002");
    }

    #[test]
    fn an_unknown_reference_lists_the_containers_that_exist() {
        let root = scratch("unknown");
        make_container(&root, "container-1730000003", "alpha");
        make_container(&root, "container-1730000004", "beta");
        let error = resolve_container_in_root(&root, "nope").unwrap_err();
        assert!(error.contains("alpha"), "{error}");
        assert!(error.contains("beta"), "{error}");
    }

    #[test]
    fn an_empty_store_says_so_instead_of_listing_nothing() {
        let root = scratch("empty");
        let error = resolve_container_in_root(&root, "anything").unwrap_err();
        assert!(error.contains("no containers yet"), "{error}");
    }

    #[test]
    fn two_containers_sharing_a_name_are_ambiguous_not_first_match() {
        let root = scratch("dupe");
        make_container(&root, "container-1730000005", "same");
        make_container(&root, "container-1730000006", "same");
        let error = resolve_container_in_root(&root, "same").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(error.contains("container-1730000005"), "{error}");
        // The list-based API is ambiguity-first as well: it must not quietly
        // pick the row a directory scan happened to visit first.
        let listed = crate::scan_containers(&root.to_string_lossy())
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        assert!(match_container(&listed, "same").is_err());
    }

    #[test]
    fn a_directory_without_container_json_is_not_resolvable() {
        let root = scratch("broken");
        fs::create_dir_all(root.join("instances/container-1730000008")).unwrap();
        let error = resolve_container_in_root(&root, "container-1730000008").unwrap_err();
        assert!(error.contains("no container named"), "{error}");
    }

    #[test]
    fn a_sealed_style_timestamp_prefix_resolves() {
        let root = scratch("sealed-prefix");
        make_container(&root, "container-1730000000-4f2a", "sealed one");
        make_container(&root, "container-1730000001-9b3c", "sealed two");
        let found = resolve_container_in_root(&root, "1730000000").unwrap();
        assert_eq!(found.id, "container-1730000000-4f2a");
        // Two ids sharing the timestamp prefix are an ambiguity, not a guess.
        make_container(&root, "container-1730000000-77ee", "sealed three");
        let error = resolve_container_in_root(&root, "1730000000").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
    }

    #[test]
    fn a_parent_reference_never_reaches_the_filesystem() {
        let root = scratch("escape");
        make_container(&root, "container-1730000007", "alpha");
        // `instances/..` is the runtime root: joining it blindly would hand
        // back the directory holding every container, and a destructive verb
        // would delete the store. The join gate refuses these before any path
        // is built, and the scan never joins the raw string — so the fallback
        // is an honest miss, not a traversal.
        for attempt in ["..", ".", "...", "..a", "a."] {
            let error = resolve_container_in_root(&root, attempt).unwrap_err();
            assert!(error.contains("no container named"), "{attempt}: {error}");
        }
    }

    #[test]
    fn a_blank_reference_is_rejected_and_a_spaced_name_resolves() {
        let root = scratch("shape");
        for attempt in ["", "   "] {
            let error = resolve_container_in_root(&root, attempt).unwrap_err();
            assert!(error.contains("expected a container"), "{attempt:?}: {error}");
        }
        // Names are only length-checked at creation: a legal name can carry
        // spaces, slashes or any Unicode, and matching it must not be
        // narrowed by the charset the id path-join needs.
        make_container(&root, "container-1730000009", "dev box/开发");
        let found = resolve_container_in_root(&root, "dev box/开发").unwrap();
        assert_eq!(found.id, "container-1730000009");
    }
