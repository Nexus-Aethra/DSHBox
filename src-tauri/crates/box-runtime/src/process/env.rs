use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
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
    clean_room: bool,
    inherited: BTreeSet<String>,
    task_overrides: BTreeMap<String, OsString>,
    protected: BTreeSet<String>,
}

impl EnvironmentPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start the child with no inherited environment except variables explicitly
    /// requested with [`Self::inherit`]. This is for deterministic toolchain
    /// work, not interactive host processes.
    pub fn clean_room(mut self) -> Self {
        self.clean_room = true;
        self
    }

    /// Preserve one parent environment variable when using [`Self::clean_room`].
    pub fn inherit(mut self, key: impl Into<String>) -> Self {
        self.inherited.insert(normalize_key(&key.into()));
        self
    }

    pub fn remove(mut self, key: impl Into<String>) -> Self {
        self.remove.insert(normalize_key(&key.into()));
        self
    }

    pub fn default_value(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.defaults
            .insert(normalize_key(&key.into()), value.into());
        self
    }

    pub fn replace(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.replace
            .insert(normalize_key(&key.into()), value.into());
        self
    }

    pub fn prepend_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.prepend_path.push(path.into());
        self
    }

    pub fn task_override(mut self, key: impl Into<String>, value: impl Into<OsString>) -> Self {
        self.task_overrides
            .insert(normalize_key(&key.into()), value.into());
        self
    }

    pub fn protect(mut self, key: impl Into<String>) -> Self {
        self.protected.insert(normalize_key(&key.into()));
        self
    }

    pub fn apply(&self, command: &mut Command) {
        if self.clean_room {
            let inherited: Vec<(String, OsString)> = self
                .inherited
                .iter()
                .filter_map(|key| env::var_os(key).map(|value| (key.clone(), value)))
                .collect();
            command.env_clear();
            for (key, value) in inherited {
                command.env(key, value);
            }
        }
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
            if !self.clean_room {
                if let Some(existing) = env::var_os("PATH") {
                    paths.extend(env::split_paths(&existing));
                }
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
    pub(crate) fn apply_to_map(
        &self,
        input: BTreeMap<String, OsString>,
    ) -> BTreeMap<String, OsString> {
        let mut result = input;
        if self.clean_room {
            result.retain(|key, _| self.inherited.iter().any(|allowed| same_key(key, allowed)));
        }
        for key in &self.remove {
            remove_map_aliases(&mut result, key);
        }
        for (key, value) in &self.defaults {
            if !result.keys().any(|current| same_key(current, key)) {
                result.insert(key.clone(), value.clone());
            }
        }
        for (key, value) in &self.replace {
            remove_map_aliases(&mut result, key);
            result.insert(key.clone(), value.clone());
        }
        if !self.prepend_path.is_empty() {
            let mut paths = self.prepend_path.clone();
            if !self.clean_room {
                if let Some(existing) = find_map_value(&result, "PATH") {
                    paths.extend(env::split_paths(existing));
                }
            }
            remove_map_aliases(&mut result, "PATH");
            if let Ok(joined) = env::join_paths(paths) {
                result.insert("PATH".to_owned(), joined);
            }
        }
        for (key, value) in &self.task_overrides {
            if !self.protected.contains(key) {
                remove_map_aliases(&mut result, key);
                result.insert(key.clone(), value.clone());
            }
        }
        result
    }
}

/// Build a deterministic policy for pnpm/npm tasks owned by DSH Box. Unlike
/// the general bundled-toolchain policy, this never inherits the user's npm,
/// pnpm, proxy, or Node configuration.
///
/// `git_dir` is the directory that contains the bundled `git` entry point
/// (the `cmd/` directory inside PortableGit). When `Some`, the directory is
/// prepended to the clean-room `PATH` and the standard Git clean-room
/// variables (`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL`, `GIT_TERMINAL_PROMPT`,
/// and a Box-owned `HOME`) are injected. When `None`, no Git-related
/// environment changes are made — this is the path taken on targets where
/// DSH Box has not yet published a bundled Git (Linux until CI ships it).
pub fn bundled_package_manager_policy(
    install_dir: Option<&Path>,
    node_dir: &Path,
    pnpm_dir: &Path,
    runtime_dir: &Path,
    npm_registry: Option<&str>,
    git_dir: Option<&Path>,
) -> Result<EnvironmentPolicy, String> {
    const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org/";
    let package_root = runtime_dir.join("pnpm");
    let config_dir = package_root.join("config");
    let home_dir = package_root.join("home");
    let app_data_dir = package_root.join("app-data");
    let local_app_data_dir = app_data_dir.join("local");
    fs::create_dir_all(&config_dir)
        .map_err(|error| format!("cannot create pnpm config directory: {error}"))?;
    fs::create_dir_all(&home_dir)
        .map_err(|error| format!("cannot create pnpm home directory: {error}"))?;
    fs::create_dir_all(&local_app_data_dir)
        .map_err(|error| format!("cannot create pnpm application-data directory: {error}"))?;
    let registry = npm_registry
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_REGISTRY);
    let npmrc = config_dir.join("npmrc");
    let global_npmrc = config_dir.join("global-npmrc");
    fs::write(&npmrc, format!("registry={registry}\n"))
        .map_err(|error| format!("cannot write managed npm configuration: {error}"))?;
    fs::write(&global_npmrc, "")
        .map_err(|error| format!("cannot write managed global npm configuration: {error}"))?;

    // Decide what HOME points at. When Git is bundled we redirect HOME to a
    // dedicated `<storage>/git/home` directory so host-side `~/.gitconfig` (or
    // the registry-backed Git config on Windows) cannot leak into the child.
    // When Git is absent, HOME stays pointed at pnpm's home directory — that
    // path is what Node/pnpm expect for their `~/.npm` / `.pnpm-state` files.
    let (effective_home, git_env) = match git_dir {
        Some(_) => {
            let git_root = runtime_dir.join("git");
            let git_home = git_root.join("home");
            let git_config_dir = git_root.join("config");
            fs::create_dir_all(&git_home)
                .map_err(|error| format!("cannot create git home directory: {error}"))?;
            fs::create_dir_all(&git_config_dir)
                .map_err(|error| format!("cannot create git config directory: {error}"))?;
            let global_gitconfig = git_config_dir.join("global.gitconfig");
            fs::write(&global_gitconfig, "")
                .map_err(|error| format!("cannot write managed global gitconfig: {error}"))?;
            (
                git_home,
                Some(GitEnvironment {
                    config_global: global_gitconfig,
                    lib_dir: git_root.join("lib"),
                }),
            )
        }
        None => (home_dir, None),
    };

    let mut policy = EnvironmentPolicy::new()
        .clean_room()
        .inherit("SystemRoot")
        .inherit("WINDIR")
        .inherit("ComSpec")
        .inherit("TEMP")
        .inherit("TMP")
        .prepend_path(node_dir)
        .prepend_path(pnpm_dir)
        .protect("DSHBOX_HOME")
        .replace("HOME", effective_home.as_os_str().to_owned())
        .replace("USERPROFILE", effective_home.as_os_str().to_owned())
        .replace("APPDATA", app_data_dir.as_os_str().to_owned())
        .replace("LOCALAPPDATA", local_app_data_dir.as_os_str().to_owned())
        .replace("NPM_CONFIG_USERCONFIG", npmrc.as_os_str().to_owned())
        .replace("NPM_CONFIG_GLOBALCONFIG", global_npmrc.as_os_str().to_owned())
        .replace("npm_config_registry", registry)
        .replace("PNPM_CONFIG_STORE_DIR", package_root.join("store").into_os_string())
        .replace("npm_config_cache", package_root.join("npm-cache").into_os_string())
        .replace("npm_config_optional", "true")
        .replace("pnpm_config_optional", "true")
        .replace("pnpm_config_verify_deps_before_run", "false");
    if let Some(directory) = git_dir {
        policy = policy.prepend_path(directory);
    }
    if let Some(git) = git_env {
        policy = policy
            .replace("GIT_CONFIG_NOSYSTEM", "1")
            .replace(
                "GIT_CONFIG_GLOBAL",
                git.config_global.as_os_str().to_owned(),
            )
            .replace("GIT_TERMINAL_PROMPT", "0");
        #[cfg(target_os = "linux")]
        {
            // Linux CI builds a private lib layout under <runtime>/git/lib;
            // PortableGit on Windows is self-contained and does not need it.
            policy = policy.replace(
                "LD_LIBRARY_PATH",
                git.lib_dir.as_os_str().to_owned(),
            );
        }
    }
    if let Some(directory) = install_dir {
        policy = policy.prepend_path(directory);
    }
    Ok(policy)
}

/// Bundle of clean-room variables that govern how the bundled `git`
/// resolves its configuration and library paths. Only constructed when
/// the caller passes a `git_dir` to `bundled_package_manager_policy`.
struct GitEnvironment {
    config_global: PathBuf,
    lib_dir: PathBuf,
}

fn normalize_key(key: &str) -> String {
    #[cfg(windows)]
    {
        key.to_ascii_uppercase()
    }
    #[cfg(not(windows))]
    {
        key.to_owned()
    }
}

fn same_key(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        left.eq_ignore_ascii_case(right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}
#[allow(dead_code)]
fn same_key_used(left: &str, right: &str) -> bool {
    same_key(left, right)
}

fn remove_env_aliases(command: &mut Command, key: &str) {
    command.env_remove(key);
    #[cfg(windows)]
    {
        let upper = key.to_ascii_uppercase();
        let lower = key.to_ascii_lowercase();
        if upper != key {
            command.env_remove(&upper);
        }
        if lower != key {
            command.env_remove(&lower);
        }
    }
}

#[cfg(test)]
fn remove_map_aliases(map: &mut BTreeMap<String, OsString>, key: &str) {
    let keys: Vec<String> = map
        .keys()
        .filter(|current| same_key(current, key))
        .cloned()
        .collect();
    for current in keys {
        map.remove(&current);
    }
}

#[cfg(test)]
fn find_map_value<'a>(map: &'a BTreeMap<String, OsString>, key: &str) -> Option<&'a OsString> {
    map.iter()
        .find(|(current, _)| same_key(current, key))
        .map(|(_, value)| value)
}

/// Rules for the bundled Node/npm/pnpm environment. When `host` is true the
/// NODE_PATH inherited from the parent is preserved (the DSH Host needs it to
/// resolve the vendored `@deepseek-ai/dsh-box-context` plugin tree); for
/// toolchain commands we strip it to avoid leaking the host machine's
/// global Node.js install into the child.
///
/// `git_dir` is prepended to `PATH` when supplied, so pnpm can invoke the
/// bundled `git` for `ADD plugin github.com/...` specs even when the host
/// PATH has no Git on it.
pub fn bundled_toolchain_policy(
    install_dir: Option<&Path>,
    node_dir: &Path,
    pnpm_dir: &Path,
    runtime_dir: Option<&Path>,
    npm_registry: Option<&str>,
    host: bool,
    git_dir: Option<&Path>,
) -> EnvironmentPolicy {
    let mut policy = EnvironmentPolicy::new()
        .remove("SHELL")
        .remove("MSYSTEM")
        .remove("TERM")
        .remove("COLORTERM")
        .prepend_path(node_dir)
        .prepend_path(pnpm_dir)
        .protect("DSHBOX_HOME")
        .protect("PNPM_CONFIG_STORE_DIR")
        .protect("npm_config_cache")
        .protect("pnpm_config_verify_deps_before_run");
    if !host {
        policy = policy.remove("NODE_PATH");
    }
    if let Some(directory) = install_dir {
        policy = policy.prepend_path(directory);
    }
    if let Some(directory) = git_dir {
        policy = policy.prepend_path(directory);
    }
    if let Some(registry) = npm_registry {
        policy = policy.replace("npm_config_registry", registry);
    }
    if let Some(runtime) = runtime_dir {
        policy = policy
            .replace(
                "PNPM_CONFIG_STORE_DIR",
                runtime.join("pnpm").join("store").into_os_string(),
            )
            .replace(
                "npm_config_cache",
                runtime.join("pnpm").join("npm-cache").into_os_string(),
            );
    }
    // DSH's lockfile contains platform-specific optional packages (notably
    // esbuild, lefthook, and koffi). A service-level `npm_config_optional`
    // inherited from the host must not turn a normal template pull into
    // `--no-optional`, which leaves Windows binaries absent and forces an
    // unavailable native build toolchain.
    policy
        .replace("npm_config_optional", "true")
        .replace("pnpm_config_optional", "true")
        .replace("pnpm_config_verify_deps_before_run", "false")
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
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    } else {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
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
        let output = EnvironmentPolicy::new()
            .protect("DSH_HOME")
            .task_override("DSH_HOME", "bad")
            .apply_to_map(input);
        assert_eq!(output.get("DSH_HOME").unwrap(), "fixed");
    }

    #[test]
    fn bundled_pnpm_policy_allows_the_managed_profile_home() {
        let mut input = BTreeMap::new();
        input.insert("DSH_HOME".to_owned(), OsString::from("host-home"));
        let output = bundled_toolchain_policy(
            None,
            Path::new("node"),
            Path::new("pnpm"),
            None,
            None,
            false,
            None,
        )
        .task_override("DSH_HOME", "container-home")
        .apply_to_map(input);
        assert_eq!(output.get("DSH_HOME").unwrap(), "container-home");
    }

    #[test]
    fn bundled_pnpm_policy_pins_the_pnpm_11_store_config() {
        let output = bundled_toolchain_policy(
            None,
            Path::new("node"),
            Path::new("pnpm"),
            Some(Path::new("box-data")),
            None,
            false,
            None,
        )
        .apply_to_map(BTreeMap::new());
        assert_eq!(
            output.get("PNPM_CONFIG_STORE_DIR").unwrap(),
            &Path::new("box-data")
                .join("pnpm")
                .join("store")
                .into_os_string()
        );
        assert_eq!(output.get("NPM_CONFIG_OPTIONAL").unwrap(), "true");
        assert_eq!(output.get("PNPM_CONFIG_OPTIONAL").unwrap(), "true");
    }

    #[test]
    fn clean_room_discards_host_package_manager_configuration() {
        let mut input = BTreeMap::new();
        input.insert(
            "NPM_CONFIG_REGISTRY".to_owned(),
            OsString::from("https://host-invalid.example/"),
        );
        input.insert(
            "NPM_CONFIG_USERCONFIG".to_owned(),
            OsString::from("host-npmrc"),
        );
        input.insert("HTTPS_PROXY".to_owned(), OsString::from("host-proxy"));
        input.insert("NODE_PATH".to_owned(), OsString::from("host-node-path"));
        input.insert("SYSTEMROOT".to_owned(), OsString::from("system-root"));
        let output = EnvironmentPolicy::new()
            .clean_room()
            .inherit("SYSTEMROOT")
            .replace("NPM_CONFIG_REGISTRY", "https://box.example/")
            .replace("NPM_CONFIG_USERCONFIG", "box-npmrc")
            .apply_to_map(input);
        assert_eq!(output.get("NPM_CONFIG_REGISTRY").unwrap(), "https://box.example/");
        assert_eq!(output.get("NPM_CONFIG_USERCONFIG").unwrap(), "box-npmrc");
        assert_eq!(output.get("SYSTEMROOT").unwrap(), "system-root");
        assert!(!output.contains_key("APPDATA"));
        assert!(!output.contains_key("LOCALAPPDATA"));
        assert!(!output.contains_key("HTTPS_PROXY"));
        assert!(!output.contains_key("NODE_PATH"));
    }

    #[test]
    fn clean_room_path_does_not_append_the_host_path() {
        let mut input = BTreeMap::new();
        input.insert(
            "PATH".to_owned(),
            env::join_paths([PathBuf::from("host-path")]).unwrap(),
        );
        let output = EnvironmentPolicy::new()
            .clean_room()
            .prepend_path("bundled-node")
            .apply_to_map(input);
        let paths: Vec<PathBuf> = env::split_paths(output.get("PATH").unwrap()).collect();
        assert_eq!(paths, vec![PathBuf::from("bundled-node")]);
    }

    #[test]
    fn prepend_path_keeps_existing_tail() {
        let mut input = BTreeMap::new();
        input.insert(
            "PATH".to_owned(),
            env::join_paths([PathBuf::from("tail")]).unwrap(),
        );
        let output = EnvironmentPolicy::new()
            .prepend_path("front")
            .apply_to_map(input);
        let values: Vec<_> = env::split_paths(output.get("PATH").unwrap()).collect();
        assert_eq!(values[0], PathBuf::from("front"));
        assert_eq!(values[1], PathBuf::from("tail"));
    }

    #[test]
    fn bundled_package_manager_policy_injects_git_path_and_clean_room_vars() {
        let mut input = BTreeMap::new();
        input.insert("PATH".to_owned(), env::join_paths([PathBuf::from("host-path")]).unwrap());
        input.insert("HOME".to_owned(), OsString::from("C:\\Users\\host"));
        input.insert(
            "GIT_CONFIG_GLOBAL".to_owned(),
            OsString::from("C:\\Users\\host\\.gitconfig"),
        );
        input.insert("HTTPS_PROXY".to_owned(), OsString::from("http://host-proxy:8080"));

        let policy = bundled_package_manager_policy(
            None,
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("box-data"),
            Some("https://registry.npmjs.org/"),
            Some(Path::new("bundled/git/cmd")),
        )
        .unwrap();
        let output = policy.apply_to_map(input);

        // Git dir is prepended to PATH, after Node/pnpm.
        let paths: Vec<PathBuf> = env::split_paths(&output["PATH"]).collect();
        assert_eq!(paths[0], PathBuf::from("bundled/node"));
        assert_eq!(paths[1], PathBuf::from("bundled/pnpm"));
        assert_eq!(paths[2], PathBuf::from("bundled/git/cmd"));
        assert!(!paths.contains(&PathBuf::from("host-path")));

        // Host HOME is replaced by the managed git HOME; the original
        // sentinel ~/.gitconfig cannot reach the child.
        let expected_home = PathBuf::from("box-data")
            .join("git")
            .join("home");
        assert_eq!(output["HOME"].as_os_str(), expected_home.as_os_str());
        let expected_global = PathBuf::from("box-data")
            .join("git")
            .join("config")
            .join("global.gitconfig");
        assert_eq!(output["GIT_CONFIG_GLOBAL"].as_os_str(), expected_global.as_os_str());
        assert_eq!(output["GIT_CONFIG_NOSYSTEM"], OsString::from("1"));
        assert_eq!(output["GIT_TERMINAL_PROMPT"], OsString::from("0"));

        // Host proxy still does not leak.
        assert!(!output.contains_key("HTTPS_PROXY"));
    }

    #[test]
    fn bundled_package_manager_policy_omits_git_when_dir_is_none() {
        let mut input = BTreeMap::new();
        input.insert("PATH".to_owned(), env::join_paths([PathBuf::from("host-path")]).unwrap());

        let policy = bundled_package_manager_policy(
            None,
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("box-data"),
            Some("https://registry.npmjs.org/"),
            None,
        )
        .unwrap();
        let output = policy.apply_to_map(input);

        let paths: Vec<PathBuf> = env::split_paths(&output["PATH"]).collect();
        assert_eq!(paths, vec![PathBuf::from("bundled/node"), PathBuf::from("bundled/pnpm")]);

        // No Git-related overrides when git_dir is absent.
        assert!(!output.contains_key("GIT_CONFIG_NOSYSTEM"));
        assert!(!output.contains_key("GIT_CONFIG_GLOBAL"));
        assert!(!output.contains_key("GIT_TERMINAL_PROMPT"));

        // HOME falls back to the pnpm-side home directory.
        let expected_home = PathBuf::from("box-data")
            .join("pnpm")
            .join("home");
        assert_eq!(output["HOME"].as_os_str(), expected_home.as_os_str());
    }
}
