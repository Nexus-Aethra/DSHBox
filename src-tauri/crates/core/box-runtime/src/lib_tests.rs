//! tests for lib.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

use super::*;
    use git2::{Repository, Signature};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn shallow_clone_uses_libgit2_not_a_git_executable() {
        let root = std::env::temp_dir().join(format!(
            "dsh-box-git-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(&source).unwrap();
        let repository = Repository::init(&source).unwrap();
        fs::write(source.join("README.md"), "source").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        let commit = shallow_clone(source.to_str().unwrap(), &destination, None).unwrap();
        assert!(destination.join("README.md").is_file());
        assert_eq!(commit.len(), 40);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_remote_tags_reads_local_repository_tags() {
        let root = std::env::temp_dir().join(format!(
            "dsh-box-lstags-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let repository = Repository::init(&root).unwrap();
        fs::write(root.join("README.md"), "tagged").unwrap();
        let mut index = repository.index().unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("DSH Box", "box@example.invalid").unwrap();
        let commit_id = repository
            .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        let commit = repository.find_commit(commit_id).unwrap();
        repository.tag("v0.1.0", commit.as_object(), &signature, "release", false).unwrap();
        repository.tag("v0.2.0", commit.as_object(), &signature, "release", false).unwrap();
        let tags = list_remote_tags(root.to_str().unwrap()).unwrap();
        assert_eq!(tags, vec!["v0.1.0".to_owned(), "v0.2.0".to_owned()]);
        let _ = fs::remove_dir_all(root);
    }
