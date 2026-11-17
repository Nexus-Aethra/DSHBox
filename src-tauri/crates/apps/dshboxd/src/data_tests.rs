//! tests for data.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use box_foundation::BoxPaths;
    use box_scheduler::TaskManager;
    use std::env;

    struct NoopNotifier;
    impl box_scheduler::TaskNotifier for NoopNotifier {
        fn stage(&self, _task_id: &str, _stage: &str, _progress: u8) {}
        fn log(&self, _task_id: &str, _line: &str) {}
    }

    fn test_root(name: &str) -> PathBuf {
        // A UUID, not now_seconds(): a wall-clock second is coarse enough
        // that a re-run inside the same second reuses the previous
        // directory, and this root is a document store another test may
        // still be reading.
        let root = env::temp_dir().join(format!(
            "dshboxd-data-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn test_task(runtime: &Path) -> TaskContext {
        TaskContext {
            manager: TaskManager::memory(),
            paths: BoxPaths {
                config: runtime.join("config.json"),
                runtime: Some(runtime.to_path_buf()),
            },
            notifier: std::sync::Arc::new(NoopNotifier),
            task_id: "test".to_owned(),
            profile_dir: None,
        }
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        fs::create_dir_all(root).unwrap();
        for (name, content) in files {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    fn local_dir_source(path: &Path) -> ParsedSource {
        ParsedSource::LocalDir {
            path: path.to_path_buf(),
        }
    }

    fn bare_name_source(name: &str) -> ParsedSource {
        ParsedSource::BareName {
            name: name.to_owned(),
            scope: None,
            version: None,
        }
    }

    fn test_container(runtime: &Path, id: &str) -> DshContainer {
        let directory = runtime.join("instances").join(id);
        fs::create_dir_all(&directory).unwrap();
        let container = DshContainer {
            id: id.to_owned(),
            name: id.to_owned(),
            version: "test".to_owned(),
            profile: "web".to_owned(),
            template: None,
            env: BTreeMap::new(),
            runtime: None,
            directory: directory.to_string_lossy().into_owned(),
            status: "stopped".to_owned(),
        };
        fs::write(
            directory.join("container.json"),
            serde_json::to_string_pretty(&container).unwrap(),
        )
        .unwrap();
        container
    }

    #[test]
    fn data_source_name_derives_a_sensible_store_key() {
        assert_eq!(
            data_source_name(&ParsedSource::Github {
                url: "https://github.com/team/media-pack".to_owned(),
                ref_: None,
            }),
            "media-pack"
        );
        assert_eq!(
            data_source_name(&ParsedSource::Tarball {
                url: "https://intranet/datasets.tgz".to_owned(),
                local: false,
            }),
            "datasets"
        );
        assert_eq!(
            data_source_name(&ParsedSource::Tarball {
                url: "./packs/audio.tar.gz".to_owned(),
                local: true,
            }),
            "audio"
        );
        assert_eq!(
            data_source_name(&ParsedSource::LocalDir {
                path: PathBuf::from("/work/datasets-v2"),
            }),
            "datasets-v2"
        );
        assert_eq!(
            data_source_name(&bare_name_source("datasets-v2")),
            "datasets-v2"
        );
    }

    #[test]
    fn import_stores_by_digest_and_deduplicates_identical_content() {
        let runtime = test_root("dedupe");
        let task = test_task(&runtime);
        let first_source = runtime.join("sources/first");
        write_tree(
            &first_source,
            &[("README.md", "hello"), ("data.bin", "\x00\x01")],
        );

        let first = import_or_resolve(&task, &runtime, &local_dir_source(&first_source)).unwrap();
        assert_eq!(first.name, "first");
        assert_eq!(first.digest.len(), 16);
        assert!(data_root(&runtime).join(&first.digest).is_dir());

        // Identical content (different source name) deduplicates to the same
        // digest and reuses the existing store directory.
        let second_source = runtime.join("sources/second");
        write_tree(
            &second_source,
            &[("README.md", "hello"), ("data.bin", "\x00\x01")],
        );
        let second = import_or_resolve(&task, &runtime, &local_dir_source(&second_source)).unwrap();
        assert_eq!(second.digest, first.digest);
        assert_ne!(second.name, first.name);

        let index = read_data_index(&runtime);
        assert_eq!(index.len(), 2);
        assert_eq!(index["first"].digest, first.digest);
        assert_eq!(index["second"].digest, first.digest);
        let staging = data_root(&runtime).join("staging");
        assert_eq!(
            fs::read_dir(&staging)
                .map(|entries| entries.count())
                .unwrap_or(0),
            0
        );
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn materialize_copies_into_container_and_records_usage() {
        let runtime = test_root("materialize");
        let task = test_task(&runtime);
        let source = runtime.join("sources/payload");
        write_tree(
            &source,
            &[("config.yaml", "k: v"), ("assets/logo.png", "png")],
        );
        let container = test_container(&runtime, "c1");

        materialize_data_add_at(&runtime, &task, &container, &local_dir_source(&source)).unwrap();

        let payload = PathBuf::from(&container.directory).join("extensions/data/payload");
        assert!(payload.join("config.yaml").is_file());
        assert!(payload.join("assets/logo.png").is_file());
        let uses = read_container_data(&container);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "payload");
        assert_eq!(uses[0].digest.len(), 16);
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn bare_name_resolves_from_index_and_errors_when_unknown() {
        let runtime = test_root("bare");
        let task = test_task(&runtime);
        let missing = import_or_resolve(&task, &runtime, &bare_name_source("ghost"));
        assert!(missing.is_err());
        assert!(missing.unwrap_err().contains("not in the data store"));

        let source = runtime.join("sources/payload");
        write_tree(&source, &[("a.txt", "a")]);
        import_or_resolve(&task, &runtime, &local_dir_source(&source)).unwrap();
        let resolved = import_or_resolve(&task, &runtime, &bare_name_source("payload")).unwrap();
        assert_eq!(resolved.name, "payload");
        assert_eq!(resolved.digest.len(), 16);
        let _ = fs::remove_dir_all(&runtime);
    }

    #[test]
    fn prune_removes_orphans_but_keeps_digests_in_use() {
        let runtime = test_root("prune");
        let task = test_task(&runtime);
        let kept_source = runtime.join("sources/kept");
        write_tree(&kept_source, &[("k.txt", "keep me")]);
        let orphan_source = runtime.join("sources/orphan");
        write_tree(&orphan_source, &[("o.txt", "orphan me")]);

        let kept = import_or_resolve(&task, &runtime, &local_dir_source(&kept_source)).unwrap();
        let orphan = import_or_resolve(&task, &runtime, &local_dir_source(&orphan_source)).unwrap();
        let container = test_container(&runtime, "c1");
        materialize_data_add_at(&runtime, &task, &container, &local_dir_source(&kept_source))
            .unwrap();

        let removed = prune_orphaned_data_at(&runtime).unwrap();
        assert!(removed.contains(&orphan.digest));
        assert!(!removed.contains(&kept.digest));
        assert!(data_root(&runtime).join(&kept.digest).is_dir());
        assert!(!data_root(&runtime).join(&orphan.digest).exists());

        // The index drops entries whose store directory was removed.
        let index = read_data_index(&runtime);
        assert!(index.contains_key("kept"));
        assert!(!index.contains_key("orphan"));

        // A second prune is a no-op (no more orphans).
        assert!(prune_orphaned_data_at(&runtime).unwrap().is_empty());
        let _ = fs::remove_dir_all(&runtime);
    }
