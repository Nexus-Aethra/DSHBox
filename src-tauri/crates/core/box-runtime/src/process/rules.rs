use super::{env::{bundled_toolchain_policy, EnvironmentPolicy}, spec::ProcessSpec};
use std::path::{Path, PathBuf};

pub fn bundled_node_spec(node: impl Into<PathBuf>, policy: EnvironmentPolicy) -> ProcessSpec {
    ProcessSpec::new(node).policy(policy)
}

pub fn bundled_toolchain_policy_for(
    install_dir: Option<&Path>,
    node_dir: &Path,
    pnpm_dir: &Path,
    runtime_dir: Option<&Path>,
    npm_registry: Option<&str>,
    host: bool,
    git_dir: Option<&Path>,
) -> EnvironmentPolicy {
    bundled_toolchain_policy(install_dir, node_dir, pnpm_dir, runtime_dir, npm_registry, host, git_dir)
}

pub fn dsh_host_spec(node: impl Into<PathBuf>, policy: EnvironmentPolicy) -> ProcessSpec {
    ProcessSpec::new(node).policy(policy).new_process_group(true)
}
