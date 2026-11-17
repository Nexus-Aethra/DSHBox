//! tests for resource_map.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use std::fs;

    fn fresh_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "dshbox-map-test-{}-{}",
            now_seconds(),
            uuid::Uuid::new_v4().simple().to_string(),
        ))
    }

    fn mk_entry(name: &str, r#type: ResourceType) -> ResourceEntry {
        ResourceEntry::new(build_resource_id(r#type, name, None), r#type, format!("store/{name}"))
    }

    #[test]
    fn add_and_get_roundtrip() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let entry = mk_entry("demo", ResourceType::Plugin);
        assert!(add_resource(&root, "demo", entry).unwrap());
        let got = get_resource(&root, "demo").unwrap();
        // `demo` (no version) → `build_resource_id` uses `plugin:` +
        // `fnv1a64_hex("demo")` — the hash is deterministic.
        assert_eq!(got.id, format!("plugin:{}", fnv1a64_hex("demo")));
        assert_eq!(got.status, ResourceStatus::Active);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fnv1a64_hex_is_deterministic_and_stable() {
        let hash_demo = fnv1a64_hex("demo");
        let hash_tag = fnv1a64_hex("dsh-v0.1.0-rc.7");
        // Sanity: non-empty string produces a 16-char hex digest.
        assert_eq!(hash_demo.len(), 16);
        assert!(hash_demo.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(hash_tag.len(), 16);
        // Empty string → FNV-1a offset basis (the seed, since no bytes
        // are XOR'd or multiplied).
        assert_eq!(fnv1a64_hex(""), "cbf29ce484222325");
        // Same input → same output.
        assert_eq!(fnv1a64_hex("a:b"), fnv1a64_hex("a:b"));
        // Different input → different output.
        assert_ne!(fnv1a64_hex("a:b"), fnv1a64_hex("a:c"));
    }

    #[test]
    fn remove_refused_when_refs_present() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        add_resource(&root, "locked", mk_entry("locked", ResourceType::Plugin)).unwrap();
        add_reference(&root, "locked", "container:123").unwrap();
        let result = remove_resource(&root, "locked");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("still referenced"));
        // Entry still active after failed remove.
        assert!(has_active_resource(&root, "locked"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remove_then_recover() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        add_resource(&root, "x", mk_entry("x", ResourceType::Template)).unwrap();

        let (id, _path) = remove_resource(&root, "x").unwrap();
        assert!(!has_active_resource(&root, "x"));

        // Enqueue for hard delete (simulated — queue is separate module).
        // Recover the resource.
        let recovered = mk_entry("x", ResourceType::Template);
        let recovered = {
            let mut e = recovered;
            e.id = id;
            e
        };
        recover_resource(&root, "x", recovered).unwrap();
        assert!(has_active_resource(&root, "x"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn add_ref_and_remove_ref() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        add_resource(&root, "res", mk_entry("res", ResourceType::Plugin)).unwrap();
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 0);

        add_reference(&root, "res", "container:a").unwrap();
        add_reference(&root, "res", "container:a").unwrap(); // idempotent
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 1);

        add_reference(&root, "res", "container:b").unwrap();
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 2);

        remove_reference(&root, "res", "container:a").unwrap();
        assert_eq!(get_resource(&root, "res").unwrap().refs.len(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let root = fresh_root();
        fs::create_dir_all(&root).unwrap();
        let state_dir = root.join("state");
        fs::create_dir_all(&state_dir).unwrap();
        assert!(read_resource_map(&root).is_empty());
        let _ = fs::remove_dir_all(root);
    }
