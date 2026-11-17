//! tests for mirror.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use git2::{Repository, Signature};

    /// A throwaway runtime root, named so two test runs never collide.
    fn temporary_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("dsh-box-mirror-{label}-{}", now_nanos()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Appends one commit to a source repository and tags it with `tag`.
    ///
    /// Creating versions *on demand* is what makes the incrementality test mean
    /// something. libgit2's local transport ignores the requested depth and
    /// packs every ref it can reach (`local.c` walks all references), so a
    /// source that already contains v2 when v1 is fetched would deliver v2's
    /// objects too and the test would pass no matter what negotiation does.
    /// Tagging after the first fetch keeps the second transfer honest.
    fn add_version(source: &Path, tag: &str, files: usize) {
        let repository = Repository::init(source).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        for index in 0..files {
            // Each version adds files, so consecutive versions share most of
            // their trees: a second fetch must carry only what it adds.
            fs::write(source.join(format!("file-{index}.txt")), format!("{tag}-{index}"))
                .unwrap();
        }
        let mut index = repository.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repository.head().ok().and_then(|head| head.target());
        let parents: Vec<git2::Commit<'_>> = parent
            .iter()
            .filter_map(|id| repository.find_commit(*id).ok())
            .collect();
        let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        let commit = repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                &format!("version {tag}"),
                &tree,
                &parent_refs,
            )
            .unwrap();
        {
            let commit = repository.find_commit(commit).unwrap();
            repository
                .tag(tag, commit.as_object(), &signature, "release", false)
                .unwrap();
        }
        // The commits above borrow `repository`, so they have to go out of
        // scope before the repository itself is released.
        drop(tree);
        drop(parents);
        drop(repository);
    }

    /// A source repository with one commit per version, tagged `v1`/`v2`.
    ///
    /// A local directory is a real remote to libgit2 (its local transport),
    /// which is what lets most of these tests run without a network.
    fn source_with_versions(root: &Path) -> PathBuf {
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let repository = Repository::init(&source).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        for (version, files) in [(1, 3usize), (2, 5usize)] {
            // Each version adds files, so the two commits share most of their
            // trees: a second fetch must transfer only what it adds.
            for index in 0..files {
                fs::write(source.join(format!("file-{index}.txt")), format!("v{version}-{index}"))
                    .unwrap();
            }
            let mut index = repository.index().unwrap();
            index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
            index.write().unwrap();
            let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repository.head().ok().and_then(|head| head.target());
            let parents: Vec<git2::Commit<'_>> = parent
                .iter()
                .filter_map(|id| repository.find_commit(*id).ok())
                .collect();
            let parent_refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
            let commit = repository
                .commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &format!("version {version}"),
                    &tree,
                    &parent_refs,
                )
                .unwrap();
            let commit = repository.find_commit(commit).unwrap();
            repository
                .tag(&format!("v{version}"), commit.as_object(), &signature, "release", false)
                .unwrap();
        }
        source
    }

    #[test]
    fn ensure_bare_mirror_creates_a_bare_repository_once() {
        let root = temporary_root("ensure");
        let source = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        assert!(mirror.is_dir());
        assert!(is_usable(&mirror));
        // A bare repository with a remote and no clone: only git's own
        // template files (config, HEAD, sample hooks) - no objects yet.
        assert!(
            mirror_size_bytes(&mirror) < 128 * 1024,
            "an empty mirror must not carry objects: {}",
            mirror_size_bytes(&mirror)
        );
        // A second call must be a no-op, not a rebuild.
        let again = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        assert_eq!(again, mirror);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fetch_ref_to_mirror_adds_a_new_version_and_repeats_for_free() {
        let root = temporary_root("incremental");
        let source = root.join("source");
        // v1 exists when the mirror is first filled; v2 is created *after* that
        // fetch, so the second transfer can only carry what v2 adds. (A local
        // transport ignores depth and packs every reachable ref, so a source that
        // already held both versions would make this test vacuous - see
        // `add_version`.)
        add_version(&source, "v1", 3);
        let remote = source.to_str().unwrap();
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, remote, &callbacks).unwrap();

        let first = fetch_ref_to_mirror(&mirror, remote, Some("v1"), &callbacks).unwrap();
        assert!(first > 0, "the first fetch must transfer the version's objects");
        let first_commit = resolve_in_mirror(&mirror, Some("v1")).unwrap();
        assert_eq!(first_commit.len(), 40);

        // v2 extends v1 with two files, so most of its tree is already here.
        add_version(&source, "v2", 5);
        let second = fetch_ref_to_mirror(&mirror, remote, Some("v2"), &callbacks).unwrap();
        assert!(
            second > 0,
            "a new version must transfer something, or the test proves nothing"
        );
        // The remote-incrementality claim (a second version carrying *less* than
        // the first) cannot be asserted through libgit2's local transport: it
        // ignores the requested depth and packs every reachable ref, so the
        // count it reports is the whole graph each time rather than a delta.
        // The network path is where negotiation actually saves the transfer, and
        // that is measured against the real repository instead: 22591 objects
        // for the first version, 1228 for the second (design doc §2).
        let second_commit = resolve_in_mirror(&mirror, Some("v2")).unwrap();
        assert_ne!(first_commit, second_commit);

        // Re-fetching a version the mirror holds transfers nothing at all, which
        // is the property a repeat pull depends on.
        let repeat = fetch_ref_to_mirror(&mirror, remote, Some("v1"), &callbacks).unwrap();
        assert_eq!(repeat, 0, "re-fetching a mirrored version must transfer nothing");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_in_mirror_reports_a_missing_version_without_network() {
        let root = temporary_root("resolve");
        let source = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        // No fetch yet: the mirror exists but holds nothing.
        let error = resolve_in_mirror(&mirror, Some("v1")).unwrap_err();
        assert!(!error.transport, "an empty mirror is not a transport failure");
        assert!(!error.message.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn checkout_from_mirror_writes_the_tree_and_borrows_the_objects() {
        let root = temporary_root("checkout");
        let source = source_with_versions(&root);
        let remote = source.to_str().unwrap();
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, remote, &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, remote, Some("v2"), &callbacks).unwrap();
        let commit = resolve_in_mirror(&mirror, Some("v2")).unwrap();

        let destination = root.join("checkout");
        checkout_from_mirror(&mirror, &destination, &commit, &callbacks).unwrap();
        // v2 added file-0..file-4 on top of v1's file-0..file-2.
        for index in 0..5 {
            assert!(
                destination.join(format!("file-{index}.txt")).is_file(),
                "the checkout must contain file-{index}.txt"
            );
        }
        assert_eq!(
            fs::read_to_string(destination.join("file-3.txt")).unwrap(),
            "v2-3"
        );
        // The `.git` is a stub borrowing the mirror, not a copy of it: that is
        // the difference between a 2.5 MB checkout and a 375 MB one.
        assert!(destination.join(".git").is_dir());
        assert_git_is_borrowed(&destination);

        // Dropping it leaves plain source, which is what gets published.
        drop_checkout_metadata(&destination);
        assert!(!destination.join(".git").exists());
        assert!(destination.join("file-0.txt").is_file());
        let _ = fs::remove_dir_all(root);
    }

    /// The whole point of the alternate: the checkout must not carry a copied
    /// object database. A cloned `.git` would hold a pack of the mirror's
    /// objects; a borrowing one holds none.
    fn assert_git_is_borrowed(destination: &Path) {
        let pack_dir = destination.join(".git").join("objects").join("pack");
        let packs: Vec<_> = fs::read_dir(&pack_dir)
            .map(|entries| entries.flatten().collect())
            .unwrap_or_default();
        assert!(
            packs.is_empty(),
            "a borrowed `.git` must not carry a pack: {packs:?}"
        );
        // The stub is git's own config/HEAD/index/refs, kilobytes not megabytes.
        assert!(
            mirror_size_bytes(&destination.join(".git")) < 128 * 1024,
            "a borrowed `.git` must stay a stub"
        );
    }

    #[test]
    fn fetch_into_a_moved_remote_url_repoints_the_mirror() {
        let root = temporary_root("repoint");
        let first = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, first.to_str().unwrap(), &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, first.to_str().unwrap(), Some("v1"), &callbacks).unwrap();
        let remote_file = mirror.join(MIRROR_SOURCE_FILE);
        assert_eq!(
            fs::read_to_string(&remote_file).unwrap().trim(),
            first.to_str().unwrap()
        );

        // A second source standing in for a changed `config set mirror` value.
        let second = root.join("second");
        fs::create_dir_all(&second).unwrap();
        let repository = Repository::init(&second).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        fs::write(second.join("other.txt"), "other").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("other.txt")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let commit = repository
            .commit(Some("HEAD"), &signature, &signature, "other", &tree, &[])
            .unwrap();
        let commit = repository.find_commit(commit).unwrap();
        repository
            .tag("other", commit.as_object(), &signature, "release", false)
            .unwrap();

        // Re-pointing must keep the objects: the old version is still resolvable.
        fetch_ref_to_mirror(&mirror, second.to_str().unwrap(), Some("other"), &callbacks).unwrap();
        assert_eq!(
            fs::read_to_string(&remote_file).unwrap().trim(),
            second.to_str().unwrap()
        );
        assert!(resolve_in_mirror(&mirror, Some("v1")).is_ok());
        assert!(resolve_in_mirror(&mirror, Some("other")).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolve_default_branch_uses_the_remotes_branch_not_the_local_default() {
        let root = temporary_root("default-branch");
        // A source whose default branch is `main`; `init_bare` on most
        // machines would point the mirror's HEAD at `master`, so resolving
        // "the default branch" must not trust HEAD alone.
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let repository = Repository::init(&source).unwrap();
        repository.set_head("refs/heads/main").unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        fs::write(source.join("README.md"), "main").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        repository
            .commit(Some("refs/heads/main"), &signature, &signature, "main", &tree, &[])
            .unwrap();

        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, source.to_str().unwrap(), None, &callbacks).unwrap();
        let commit = resolve_in_mirror(&mirror, None).unwrap();
        assert_eq!(commit.len(), 40);
        // And it must be the commit that is actually on `main`.
        let expected = repository.find_reference("refs/heads/main").unwrap().target().unwrap();
        assert_eq!(commit, expected.to_string());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn purge_removes_the_mirror_and_reports_the_bytes_freed() {
        let root = temporary_root("purge");
        let source = source_with_versions(&root);
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, source.to_str().unwrap(), &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, source.to_str().unwrap(), Some("v1"), &callbacks).unwrap();
        assert!(mirror_size_bytes(&mirror) > 0);
        let freed = purge(&root).unwrap();
        assert!(freed > 0);
        assert!(!mirror_path(&root).exists());
        // Purging an absent mirror is not an error: it already has the answer.
        assert_eq!(purge(&root).unwrap(), 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_cancelled_fetch_leaves_the_existing_mirror_alone() {
        let root = temporary_root("cancel");
        let source = source_with_versions(&root);
        let remote = source.to_str().unwrap();
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, remote, &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, remote, Some("v1"), &callbacks).unwrap();
        // Now cancel: the transfer must abort, and the mirror that already holds
        // a usable version must survive it.
        let cancelled = || true;
        let cancelling = MirrorCallbacks::new(&cancelled);
        let _ = fetch_ref_to_mirror(&mirror, remote, Some("v2"), &cancelling);
        assert!(is_usable(&mirror));
        assert!(resolve_in_mirror(&mirror, Some("v1")).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_cancelled_checkout_does_not_finish_the_tree() {
        let root = temporary_root("cancel-checkout");
        let source = source_with_versions(&root);
        let remote = source.to_str().unwrap();
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, remote, &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, remote, Some("v1"), &callbacks).unwrap();
        let commit = resolve_in_mirror(&mirror, Some("v1")).unwrap();

        let cancelled = || true;
        let cancelling = MirrorCallbacks::new(&cancelled);
        let destination = root.join("cancelled-checkout");
        // The notification callback must actually be reached: without
        // `notify_on(ALL)` libgit2 skips it (notify flags default to none), the
        // checkout runs to completion, and `cancel` silently does nothing. The
        // real assertion is therefore that the call reports a cancellation
        // instead of succeeding.
        let outcome = checkout_from_mirror(&mirror, &destination, &commit, &cancelling);
        assert!(
            outcome.is_err(),
            "a cancelled checkout must report a failure, not complete silently"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_tag_the_remote_never_advertised_is_not_a_reason_to_rebuild() {
        let root = temporary_root("absent-tag");
        let source = source_with_versions(&root);
        let remote = source.to_str().unwrap();
        let never = || false;
        let callbacks = MirrorCallbacks::new(&never);
        let mirror = ensure_bare_mirror(&root, remote, &callbacks).unwrap();
        fetch_ref_to_mirror(&mirror, remote, Some("v1"), &callbacks).unwrap();

        // A fetch of an explicit refspec for a tag that does not exist returns
        // Ok with zero objects - measured against the real remote - so "the
        // fetch succeeded but the ref is missing" is also what a typo looks like.
        let absent = fetch_ref_to_mirror(&mirror, remote, Some("v9-typo"), &callbacks).unwrap();
        assert_eq!(absent, 0, "a missing tag must transfer nothing");

        // The distinction that protects the cache: the remote does not advertise
        // it, so a caller must report a miss instead of rebuilding.
        let advertised = remote_has_revision(&mirror, remote, Some("v9-typo")).unwrap();
        assert!(!advertised, "the remote does not advertise a tag it never had");
        let real = remote_has_revision(&mirror, remote, Some("v1")).unwrap();
        assert!(real, "the remote does advertise a tag it has");

        // And the mirror still serves the version it held all along.
        assert!(resolve_in_mirror(&mirror, Some("v1")).is_ok());
        assert!(is_usable(&mirror));
        let _ = fs::remove_dir_all(root);
    }
