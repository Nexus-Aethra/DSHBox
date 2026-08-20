use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Environment changes applied to a child process while preserving the parent
/// environment by default.
#[derive(Debug, Clone, Default)]
pub struct EnvironmentPolicy {
    remove: BTreeSet<String>,
    defaults: BTreeMap<String, OsString>,
    replace: BTreeMap<String, OsString>,
    prepend_path: Vec<PathBuf>,
    task_overrides: BTreeMap<String, OsString>,
    protected: BTreeSet<String>,
}

impl EnvironmentPolicy {
    pub fn new() -> Self { Self::default() }

    pub fn remove(mut self, key: impl Into<String>) -> Self {
        self.remove.insert(normalize_key(&key.into()));
        self
    }

    pub fn default_value(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.defaults.insert(normalize_key(&key.into()), value.into());
        self
    }

    pub fn replace(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.replace.insert(normalize_key(&key.into()), value.into());
        self
    }

    pub fn prepend_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.prepend_path.push(path.into());
        self
    }

    pub fn task_override(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.task_overrides.insert(normalize_key(&key.into()), value.into());
        self
    }

    pub fn protect(mut self, key: impl Into<String>) -> Self {
        self.protected.insert(normalize_key(&key.into()));
        self
    }

    pub fn apply(&self, command: &mut Command) {
        for key in &self.remove {
            remove_env_aliases(command, key);
        }
        for (key, value) in &self.defaults {
            if env::var_os(key).is_none() {
                remove_env_aliases(command, key);
                command.env(key, value);
            }
        }
        for (key, value) in &self.replace {
            remove_env_aliases(command, key);
            command.env(key, value);
        }
        if !self.prepend_path.is_empty() {
            let mut paths = self.prepend_path.clone();
            if let Some(existing) = env::var_os("PATH") {
                paths.extend(env::split_paths(&existing));
            }
            remove_env_aliases(command, "PATH");
            if let Ok(joined) = env::join_paths(paths) {
                command.env("PATH", joined);
            }
        }
        for (key, value) in &self.task_overrides {
            if !self.protected.contains(key) {
                remove_env_aliases(command, key);
                command.env(key, value);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn apply_to_map(&self, input: BTreeMap<String, OsString>) -> BTreeMap<String, OsString> {
        let mut result = input;
        for key in &self.remove { remove_map_aliases(&mut result, key); }
        for (key, value) in &self.defaults {
            if !result.keys().any(|current| same_key(current, key)) { result.insert(key.clone(), value.clone()); }
        }
        for (key, value) in &self.replace {
            remove_map_aliases(&mut result, key);
            result.insert(key.clone(), value.clone());
        }
        if !self.prepend_path.is_empty() {
            let mut paths = self.prepend_path.clone();
            if let Some(existing) = find_map_value(&result, "PATH") { paths.extend(env::split_paths(existing)); }
            remove_map_aliases(&mut result, "PATH");
            if let Ok(joined) = env::join_paths(paths) { result.insert("PATH".to_owned(), joined); }
        }
        for (key, value) in &self.task_overrides {
            if !self.protected.contains(key) { remove_map_aliases(&mut result, key); result.insert(key.clone(), value.clone()); }
        }
        result
    }
}

fn normalize_key(key: &str) -> String {
    #[cfg(windows)] { key.to_ascii_uppercase() }
    #[cfg(not(windows))] { key.to_owned() }
}

fn same_key(left: &str, right: &str) -> bool {
    #[cfg(windows)] { left.eq_ignore_ascii_case(right) }
    #[cfg(not(windows))] { left == right }
}
#[allow(dead_code)]
fn same_key_used(left: &str, right: &str) -> bool { same_key(left, right) }

fn remove_env_aliases(command: &mut Command, key: &str) {
    command.env_remove(key);
    #[cfg(windows)] {
        let upper = key.to_ascii_uppercase();
        let lower = key.to_ascii_lowercase();
        if upper != key { command.env_remove(&upper); }
        if lower != key { command.env_remove(&lower); }
    }
}

#[cfg(test)]
fn remove_map_aliases(map: &mut BTreeMap<String, OsString>, key: &str) {
    let keys: Vec<String> = map.keys().filter(|current| same_key(current, key)).cloned().collect();
    for current in keys { map.remove(&current); }
}

#[cfg(test)]
fn find_map_value<'a>(map: &'a BTreeMap<String, OsString>, key: &str) -> Option<&'a OsString> {
    map.iter().find(|(current, _)| same_key(current, key)).map(|(_, value)| value)
}

/// Rules for the bundled Node/npm/pnpm environment. When `host` is true the
/// NODE_PATH inherited from the parent is preserved (the DSH Host needs it to
/// resolve the vendored `@deepseek-ai/dsh-box-context` plugin tree); for
/// toolchain commands we strip it to avoid leaking the host machine's
/// global Node.js install into the child.
pub fn bundled_toolchain_policy(
    install_dir: Option<&Path>,
    node_dir: &Path,
    pnpm_dir: &Path,
    runtime_dir: Option<&Path>,
    npm_registry: Option<&str>,
    host: bool,
) -> EnvironmentPolicy {
    let mut policy = EnvironmentPolicy::new()
        .remove("SHELL")
        .remove("MSYSTEM")
        .remove("TERM")
        .remove("COLORTERM")
        .prepend_path(node_dir)
        .prepend_path(pnpm_dir)
        .protect("DSHBOX_HOME")
        .protect("DSH_HOME")
        .protect("PNPM_STORE_DIR")
        .protect("npm_config_cache")
        .protect("pnpm_config_verify_deps_before_run");
    if !host { policy = policy.remove("NODE_PATH"); }
    if let Some(directory) = install_dir { policy = policy.prepend_path(directory); }
    if let Some(registry) = npm_registry { policy = policy.replace("npm_config_registry", registry); }
    if let Some(runtime) = runtime_dir {
        policy = policy
            .replace("PNPM_STORE_DIR", runtime.join("pnpm").join("store").into_os_string())
            .replace("npm_config_cache", runtime.join("pnpm").join("npm-cache").into_os_string());
    }
    policy.replace("pnpm_config_verify_deps_before_run", "false")
}

/// Rules for a DSH Host child. The host does not touch pnpm store / npm
/// cache (those are toolchain concerns), but does need a clean PATH and
/// the Cordis loader's vendored node_modules on NODE_PATH. Hosts own their
/// NODE_PATH and CHOKIDAR_USEPOLLING values; both are placed in `replace`
/// so subsequent `task_override` cannot clobber them by accident.
pub fn dsh_host_policy(
    node_dir: &Path,
    pnpm_dir: &Path,
    plugins_node_modules: &Path,
) -> EnvironmentPolicy {
    EnvironmentPolicy::new()
        .remove("SHELL")
        .remove("MSYSTEM")
        .remove("TERM")
        .remove("COLORTERM")
        .prepend_path(node_dir)
        .prepend_path(pnpm_dir)
        .protect("DSHBOX_HOME")
        .replace("NODE_PATH", plugins_node_modules.as_os_str().to_owned())
        .replace("CHOKIDAR_USEPOLLING", "true")
}

/// Apply a policy to a command and use hidden console defaults for Windows.
pub fn configure_command(command: &mut Command, policy: &EnvironmentPolicy) {
    policy.apply(command);
    super::platform::configure_non_interactive(command, false);
}

pub fn configure_stdio(command: &mut Command, capture: bool) {
    if capture {
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_preserve_unknown_and_replace_known_values() {
        let mut input = BTreeMap::new();
        input.insert("KEEP".to_owned(), OsString::from("yes"));
        input.insert("REGISTRY".to_owned(), OsString::from("old"));
        let policy = EnvironmentPolicy::new()
            .replace("REGISTRY", "new")
            .remove("DROP")
            .task_override("KEEP", "no");
        let output = policy.apply_to_map(input);
        assert_eq!(output.get("KEEP").unwrap(), "no");
        assert_eq!(output.get("REGISTRY").unwrap(), "new");
    }

    #[test]
    fn protected_task_override_is_ignored() {
        let mut input = BTreeMap::new();
        input.insert("DSH_HOME".to_owned(), OsString::from("fixed"));
        let output = EnvironmentPolicy::new().protect("DSH_HOME").task_override("DSH_HOME", "bad").apply_to_map(input);
        assert_eq!(output.get("DSH_HOME").unwrap(), "fixed");
    }

    #[test]
    fn prepend_path_keeps_existing_tail() {
        let mut input = BTreeMap::new();
        input.insert("PATH".to_owned(), env::join_paths([PathBuf::from("tail")]).unwrap());
        let output = EnvironmentPolicy::new().prepend_path("front").apply_to_map(input);
        let values: Vec<_> = env::split_paths(output.get("PATH").unwrap()).collect();
        assert_eq!(values[0], PathBuf::from("front"));
        assert_eq!(values[1], PathBuf::from("tail"));
    }
}
