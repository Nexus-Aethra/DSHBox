//! Controlled execution primitives for feature crates. Tauri is intentionally absent.

use box_foundation::BoxResult;
use git2::{build::RepoBuilder, FetchOptions, RemoteCallbacks};
use std::{
    path::Path,
    process::{Command, Output},
};

pub trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        executable: &Path,
        arguments: &[String],
        working_directory: Option<&Path>,
    ) -> BoxResult<Output>;
}

#[derive(Default)]
pub struct NativeProcessRunner;
impl ProcessRunner for NativeProcessRunner {
    fn run(
        &self,
        executable: &Path,
        arguments: &[String],
        working_directory: Option<&Path>,
    ) -> BoxResult<Output> {
        let mut command = Command::new(executable);
        command.args(arguments);
        if let Some(directory) = working_directory {
            command.current_dir(directory);
        }
        command
            .output()
            .map_err(|error| format!("cannot run {}: {error}", executable.display()))
    }
}

/// Clone one public revision without requiring a Git executable on PATH.
pub fn shallow_clone(url: &str, destination: &Path, revision: Option<&str>) -> BoxResult<String> {
    shallow_clone_with_cancel(url, destination, revision, || false)
}

/// Clone a public revision and stop the libgit2 transfer when cancellation is requested.
pub fn shallow_clone_with_cancel(
    url: &str,
    destination: &Path,
    revision: Option<&str>,
    cancelled: impl Fn() -> bool + Send + 'static,
) -> BoxResult<String> {
    let mut fetch = FetchOptions::new();
    // libgit2's local transport does not implement shallow fetches. Local
    // repositories are useful for tests and development; public remotes stay shallow.
    if !Path::new(url).is_dir() {
        fetch.depth(1);
    }
    let mut callbacks = RemoteCallbacks::new();
    callbacks.transfer_progress(move |_| !cancelled());
    fetch.remote_callbacks(callbacks);
    let mut builder = RepoBuilder::new();
    builder.fetch_options(fetch);
    if let Some(revision) = revision {
        builder.branch(revision);
    }
    let repository = builder
        .clone(url, destination)
        .map_err(|error| format!("Git clone failed: {error}"))?;
    let commit = repository
        .head()
        .map_err(|error| format!("cannot resolve cloned revision: {error}"))?
        .peel_to_commit()
        .map_err(|error| format!("cannot resolve cloned commit: {error}"))?;
    Ok(commit.id().to_string())
}

/// Best-effort cleanup after a failed checkout.
pub fn remove_checkout(destination: &Path) {
    if destination.exists() {
        let _ = std::fs::remove_dir_all(destination);
    }
}

#[cfg(test)]
mod tests {
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
}
