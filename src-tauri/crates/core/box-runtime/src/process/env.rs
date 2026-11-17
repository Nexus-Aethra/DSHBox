use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
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
    normalize_locale: bool,
    inherited: BTreeSet<String>,
    inherited_suffixes: Vec<String>,
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

    /// Preserve every parent environment variable whose name ends with
    /// `suffix` when using [`Self::clean_room`]. For named families of
    /// caller credentials that are user intent rather than sandbox state
    /// — the host policy uses `_API_KEY` so a provider key exported in
    /// the user's shell still reaches the container, while an agent
    /// sandbox's internal variables (which do not end in `_API_KEY`)
    /// stay out. Matching honours the platform's case rules (`same_key`).
    pub fn inherit_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.inherited_suffixes.push(suffix.into());
        self
    }

    /// Apply container-declared environment (`ENV` directives in a
    /// Boxfile, carried by the template manifest into `container.json`).
    /// Declared values replace whatever the whitelist passed through,
    /// and are applied before `task_override`, so Box's own variables
    /// (`DSH_HOME`, ...) still win over a declaration naming them.
    pub fn declared_env(mut self, env: &BTreeMap<String, String>) -> Self {
        for (key, value) in env {
            self = self.replace(key.clone(), value.clone());
        }
        self
    }

    /// Drop a C-family `LC_ALL`/`LC_CTYPE` when `LANG` names a real UTF-8
    /// locale. uutils coreutils resolves the encoding from the locale
    /// *name* and treats the whole C/POSIX family (including `C.UTF-8`) as
    /// ASCII, so a `C.UTF-8` `LC_ALL` shadowing `LANG=zh_CN.UTF-8` escapes
    /// every non-ASCII filename on a TTY. When `LANG` is unset or itself
    /// C-family the values are kept — they are the only locale signal the
    /// child has. Takes effect together with `inherit("LANG")` (and
    /// `inherit("LC_ALL")` / `inherit("LC_CTYPE")`).
    pub fn normalize_c_locale(mut self) -> Self {
        self.normalize_locale = true;
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
            let mut inherited: Vec<(String, OsString)> = self
                .inherited
                .iter()
                .filter_map(|key| env::var_os(key).map(|value| (key.clone(), value)))
                .collect();
            if !self.inherited_suffixes.is_empty() {
                for (key, value) in env::vars_os() {
                    let key = key.to_string_lossy().into_owned();
                    if self
                        .inherited
                        .iter()
                        .any(|existing| same_key(existing, &key))
                    {
                        continue;
                    }
                    if self
                        .inherited_suffixes
                        .iter()
                        .any(|suffix| key_matches_suffix(&key, suffix))
                    {
                        inherited.push((key, value));
                    }
                }
            }
            command.env_clear();
            for (key, value) in inherited {
                command.env(key, value);
            }
        }
        if self.normalize_locale {
            // The `LANG` the child actually sees: the whitelisted parent
            // value under clean_room, the parent value otherwise.
            let lang = if self.clean_room {
                self.inherited
                    .iter()
                    .any(|key| same_key(key, "LANG"))
                    .then(|| env::var_os("LANG"))
                    .flatten()
            } else {
                env::var_os("LANG")
            };
            if lang
                .as_ref()
                .map(|value| locale_names_utf8(value))
                .unwrap_or(false)
            {
                for key in ["LC_ALL", "LC_CTYPE"] {
                    let is_c_family = env::var_os(key)
                        .map(|value| locale_is_c_family(&value))
                        .unwrap_or(false);
                    if is_c_family {
                        remove_env_aliases(command, key);
                    }
                }
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
            // A clean-room policy that whitelists PATH still appends the
            // parent PATH after the prepends: a container host shares the
            // host filesystem, so the shell it serves needs the system
            // binaries. A clean-room policy that does not whitelist PATH
            // (the toolchain builds) gets the prepends only.
            let append_parent_path =
                !self.clean_room || self.inherited.iter().any(|key| same_key(key, "PATH"));
            if append_parent_path {
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
            result.retain(|key, _| {
                self.inherited.iter().any(|allowed| same_key(key, allowed))
                    || self
                        .inherited_suffixes
                        .iter()
                        .any(|suffix| key_matches_suffix(key, suffix))
            });
        }
        if self.normalize_locale {
            let lang_is_real_utf8 = find_map_value(&result, "LANG")
                .map(|value| locale_names_utf8(value))
                .unwrap_or(false);
            if lang_is_real_utf8 {
                for key in ["LC_ALL", "LC_CTYPE"] {
                    let is_c_family = find_map_value(&result, key)
                        .map(|value| locale_is_c_family(value))
                        .unwrap_or(false);
                    if is_c_family {
                        remove_map_aliases(&mut result, key);
                    }
                }
            }
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
            // See `apply`: a whitelisted PATH is appended to, a
            // non-whitelisted one is not.
            let append_parent_path =
                !self.clean_room || self.inherited.iter().any(|key| same_key(key, "PATH"));
            if append_parent_path {
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
/// pnpm, or Node configuration.
///
/// `inherit_proxy` decides whether the host's proxy aliases reach the child.
/// A registry a machine can only reach through a corporate proxy stays
/// unreachable in a fully hermetic child, so the default inherits them; a
/// caller that wants the install cut off from any proxy passes `false`.
///
/// `git_dir` is the directory that contains the bundled `git` entry point
/// (the `cmd/` directory inside PortableGit, or `bin/git` on a future Linux
/// bundle). When `Some`, that directory is prepended to the clean-room
/// `PATH`.
///
/// `host_git_dir` is the Linux-only fallback path to a host-installed `git`
/// when the bundled distribution is absent for the current target. The
/// directory is still prepended to `PATH` so pnpm's `git ls-remote` resolves,
/// but the same clean-room variables (`GIT_CONFIG_NOSYSTEM`,
/// `GIT_CONFIG_GLOBAL`, `GIT_TERMINAL_PROMPT`, plus the Box-owned `HOME`
/// and `XDG_CONFIG_HOME`) are injected so the host `~/.gitconfig` and
/// `XDG_CONFIG_HOME/git/config` cannot leak into the child.
///
/// When both are `None`, the function returns `Err` with a remediation
/// message — the clean-room policy never silently leaves git unresolvable.
pub fn bundled_package_manager_policy(
    install_dir: Option<&Path>,
    node_dir: &Path,
    pnpm_dir: &Path,
    runtime_dir: &Path,
    npm_registry: Option<&str>,
    git_dir: Option<&Path>,
    host_git_dir: Option<&Path>,
    inherit_proxy: bool,
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

    // Three branches for git availability:
    //
    // - bundled (`git_dir` is `Some`): full GitEnvironment including the
    //   private lib layout; HOME redirects to <storage>/git/home.
    // - host fallback (`git_dir` is `None`, `host_git_dir` is `Some`):
    //   same GIT_* isolation as the bundled branch, but no `lib_dir`
    //   (the host system loader resolves .so files) and no
    //   `LD_LIBRARY_PATH` injection. HOME still redirects so host
    //   `~/.gitconfig` cannot leak.
    // - absent: clean-room runs without git. Return Err early so the
    //   caller surfaces a user-visible message instead of letting pnpm
    //   fail with `git: not found`.
    let git_state = match (git_dir, host_git_dir) {
        (Some(directory), _) => {
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
            GitState::Bundled {
                path: directory.to_path_buf(),
                home: git_home,
                config_global: global_gitconfig,
                lib_dir: git_root.join("lib"),
            }
        }
        (None, Some(directory)) => {
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
            GitState::HostPassthrough {
                path: directory.to_path_buf(),
                home: git_home,
                config_global: global_gitconfig,
            }
        }
        (None, None) => {
            return Err(
                "DSH Box requires git on this platform: install it via your package manager \
                 (apt install git, dnf install git, etc.) or ship a bundled Git distribution \
                 for this target via runtime-lock.json."
                    .to_owned(),
            );
        }
    };

    let effective_home = match &git_state {
        GitState::Bundled { home, .. } | GitState::HostPassthrough { home, .. } => home.clone(),
    };

    let mut policy = EnvironmentPolicy::new()
        .clean_room()
        // Windows-only: cmd.exe, batch scripts, Defender-aware tools rely
        // on these to find the system shell.
        .inherit("SystemRoot")
        .inherit("WINDIR")
        .inherit("ComSpec")
        .inherit("TEMP")
        .inherit("TMP")
        // Linux-only: locale + timezone + ssh-agent + XDG runtime dirs.
        // Without LANG/LC_ALL, git emits "fatal: unable to auto-detect
        // email address" in some flows; without TZ, timestamps drift to
        // UTC and confuse build manifests that compare local time;
        // SSH_AUTH_SOCK lets git push via ssh; XDG_RUNTIME_DIR is
        // required by some credential helpers. We always opt in on
        // Linux, never on Windows where the names above are unused.
        .inherit("LANG")
        .inherit("LC_ALL")
        .inherit("LC_CTYPE")
        .inherit("TZ")
        .inherit("SSH_AUTH_SOCK")
        .inherit("SSH_AGENT_PID")
        .inherit("XDG_RUNTIME_DIR")
        .prepend_path(node_dir)
        .prepend_path(pnpm_dir)
        .protect("DSHBOX_HOME")
        .replace("HOME", effective_home.as_os_str().to_owned())
        .replace("USERPROFILE", effective_home.as_os_str().to_owned())
        .replace("APPDATA", app_data_dir.as_os_str().to_owned())
        .replace("LOCALAPPDATA", local_app_data_dir.as_os_str().to_owned())
        .replace("NPM_CONFIG_USERCONFIG", npmrc.as_os_str().to_owned())
        .replace(
            "NPM_CONFIG_GLOBALCONFIG",
            global_npmrc.as_os_str().to_owned(),
        )
        .replace("npm_config_registry", registry)
        .replace(
            "PNPM_CONFIG_STORE_DIR",
            package_root.join("store").into_os_string(),
        )
        .replace(
            "npm_config_cache",
            package_root.join("npm-cache").into_os_string(),
        )
        .replace("npm_config_optional", "true")
        .replace("pnpm_config_optional", "true")
        .replace("pnpm_config_verify_deps_before_run", "false");

    // Proxy variables: developers in regions with restricted GitHub / npm
    // registry access set these in their shell rc to reach a mirror, and a
    // machine whose registry is only reachable through one needs them. Both
    // the upper- and lower-case forms are honoured because curl, git, and
    // pnpm each reach for different casings on different platforms. They are
    // listed explicitly because clean_room() drops anything not inherited.
    if inherit_proxy {
        for alias in [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "https_proxy",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ] {
            policy = policy.inherit(alias);
        }
    }

    match &git_state {
        GitState::Bundled { path, .. } | GitState::HostPassthrough { path, .. } => {
            policy = policy.prepend_path(path);
        }
    }
    match &git_state {
        GitState::Bundled {
            config_global,
            #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
            lib_dir,
            ..
        } => {
            policy = policy
                .replace("GIT_CONFIG_NOSYSTEM", "1")
                .replace("GIT_CONFIG_GLOBAL", config_global.as_os_str().to_owned())
                .replace("GIT_TERMINAL_PROMPT", "0");
            #[cfg(target_os = "linux")]
            {
                // Linux CI builds a private lib layout under <runtime>/git/lib;
                // PortableGit on Windows is self-contained and does not need it.
                policy = policy.replace("LD_LIBRARY_PATH", lib_dir.as_os_str().to_owned());
            }
        }
        GitState::HostPassthrough { config_global, .. } => {
            policy = policy
                .replace("GIT_CONFIG_NOSYSTEM", "1")
                .replace("GIT_CONFIG_GLOBAL", config_global.as_os_str().to_owned())
                .replace("GIT_TERMINAL_PROMPT", "0");
            // XDG_CONFIG_HOME keeps Linux git from reading
            // $XDG_CONFIG_HOME/git/config or $HOME/.config/git/config.
            policy = policy.replace(
                "XDG_CONFIG_HOME",
                config_global.parent().unwrap().as_os_str().to_owned(),
            );
        }
    }
    if let Some(directory) = install_dir {
        policy = policy.prepend_path(directory);
    }
    Ok(policy)
}

/// Whether the clean-room child can reach `git` and which isolation
/// regime applies. `Bundled` carries the private lib layout for CI-built
/// Linux bundles; `HostPassthrough` is the Linux-only fallback that
/// uses the host's binary while still isolating git's config + state.
enum GitState {
    Bundled {
        path: PathBuf,
        home: PathBuf,
        config_global: PathBuf,
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        lib_dir: PathBuf,
    },
    HostPassthrough {
        path: PathBuf,
        home: PathBuf,
        config_global: PathBuf,
    },
}

/// Whether a locale value names the C/POSIX family — the portable
/// default, optionally with a `.UTF-8` suffix (`C`, `POSIX`, `C.UTF-8`,
/// ...). uutils coreutils resolves the encoding from the locale *name*
/// and treats the entire family as ASCII, so a C-family `LC_ALL` that
/// shadows a real UTF-8 `LANG` escapes every non-ASCII filename on a
/// TTY.
fn locale_is_c_family(value: &OsStr) -> bool {
    let upper = value.to_string_lossy().trim().to_ascii_uppercase();
    let base = upper
        .strip_suffix(".UTF-8")
        .or_else(|| upper.strip_suffix(".UTF8"))
        .unwrap_or(upper.as_str());
    matches!(base, "C" | "POSIX")
}

/// Whether a locale value names a real UTF-8 locale outside the C/POSIX
/// family — `zh_CN.UTF-8`, `en_US.utf8`, ...
fn locale_names_utf8(value: &OsStr) -> bool {
    let upper = value.to_string_lossy().trim().to_ascii_uppercase();
    (upper.contains("UTF-8") || upper.contains("UTF8")) && !locale_is_c_family(value)
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

/// Whether `key` ends with `suffix` under the platform's case rules
/// (see [`same_key`]).
fn key_matches_suffix(key: &str, suffix: &str) -> bool {
    #[cfg(windows)]
    {
        key.to_ascii_uppercase()
            .ends_with(&suffix.to_ascii_uppercase())
    }
    #[cfg(not(windows))]
    {
        key.ends_with(suffix)
    }
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
/// resolve the vendored `@nexus-aethra/dsh-box-sandbox` plugin tree); for
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
    // Always pin pnpm/npm's on-disk caches to a Box-owned directory so the
    // host's `~/.local/share/pnpm/store` (which may contain stale or
    // cross-project links under a Windows-style absolute path on Linux
    // dev machines) cannot leak into toolchain children. When the caller
    // supplies a runtime root we use <root>/pnpm/{store,npm-cache}; when
    // it doesn't, we fall back to <pnpm_dir>/../_store and _npm_cache so
    // tests still see a deterministic cache directory without making
    // permanent changes to the runtime tree.
    let store_root = runtime_dir
        .map(|root| root.join("pnpm").join("store"))
        .unwrap_or_else(|| pnpm_dir.join("..").join("_dsh-store"));
    let cache_root = runtime_dir
        .map(|root| root.join("pnpm").join("npm-cache"))
        .unwrap_or_else(|| pnpm_dir.join("..").join("_dsh-npm-cache"));
    policy = policy
        .replace("PNPM_CONFIG_STORE_DIR", store_root.into_os_string())
        .replace("npm_config_cache", cache_root.into_os_string());
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
///
/// The host is a clean room with an explicit whitelist, never a
/// passthrough of whatever the daemon happens to be running with. A
/// long-lived daemon started from an agent sandbox would otherwise leak
/// the caller's internal state (`CODEX_*` session ids, internal socket
/// paths, ...) into every container it ever starts, and a `C.UTF-8`
/// `LC_ALL` would shadow a real UTF-8 `LANG` and make uutils-coreutils
/// escape every non-ASCII filename on the terminal. Credentials reach a
/// container through the credentials resource, not through the caller's
/// ambient environment.
pub fn dsh_host_policy(
    node_dir: &Path,
    pnpm_dir: &Path,
    plugins_node_modules: &Path,
) -> EnvironmentPolicy {
    // The DSH host web server and any node-based client it spawns read
    // `HTTP_PROXY` / `HTTPS_PROXY` and route loopback fetches through a
    // developer-side proxy. The proxy answers 502 for another process's
    // 127.0.0.1 listener, so the host thinks its own webview is broken
    // and self-terminates with exit status 0 shortly after printing
    // "opening the default browser". Proxy aliases are therefore not on
    // the whitelist (and are removed again below as defence in depth) so
    // loopback stays loopback; pnpm still has its own policy
    // (`bundled_package_manager_policy`) that explicitly inherits these
    // so registry fetches keep working.
    EnvironmentPolicy::new()
        .clean_room()
        // Windows-only: cmd.exe, batch scripts and Defender-aware tools
        // need these to find the system shell.
        .inherit("SystemRoot")
        .inherit("WINDIR")
        .inherit("ComSpec")
        .inherit("TEMP")
        .inherit("TMP")
        // Locale, timezone, ssh-agent and XDG runtime dirs — the same
        // set the package-manager policy opts into. `normalize_c_locale`
        // drops a C-family `LC_ALL`/`LC_CTYPE` that would otherwise
        // shadow a real UTF-8 `LANG` (uutils coreutils reads the locale
        // *name* and treats C.UTF-8 as ASCII).
        .inherit("LANG")
        .inherit("LC_ALL")
        .inherit("LC_CTYPE")
        .inherit("TZ")
        .inherit("SSH_AUTH_SOCK")
        .inherit("SSH_AGENT_PID")
        .inherit("XDG_RUNTIME_DIR")
        // Identity and terminal type for the interactive shell the host
        // serves to the webview.
        .inherit("HOME")
        .inherit("USER")
        .inherit("LOGNAME")
        .inherit("TERM")
        // Provider credentials are user intent, not sandbox state: a key
        // exported in the user's shell (`OPENAI_API_KEY`, ...) still
        // reaches the container. The name family is the whole rule —
        // an agent sandbox's internals (`CODEX_*`, ...) match nothing.
        // A container can also declare its own `ENV` entries, which
        // `declared_env` applies on top.
        .inherit_suffix("_API_KEY")
        // A container shares the host filesystem, so the shell it serves
        // still needs the system binaries: PATH is whitelisted *and*
        // prepended to.
        .inherit("PATH")
        .normalize_c_locale()
        .remove("SHELL")
        .remove("MSYSTEM")
        .remove("COLORTERM")
        .remove("HTTP_PROXY")
        .remove("HTTPS_PROXY")
        .remove("http_proxy")
        .remove("https_proxy")
        .remove("ALL_PROXY")
        .remove("all_proxy")
        .remove("NO_PROXY")
        .remove("no_proxy")
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
#[path = "env_tests.rs"]
mod tests;
