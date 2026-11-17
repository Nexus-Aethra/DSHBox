//! tests for env.rs.
//!
//! Extracted from the source file so both halves stay readable: the module
//! under test and the cases that pin it. See AGENTS.md.

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
        use std::ffi::OsStr;
        // `normalize_key` upper-cases on Windows and leaves the
        // spelling alone elsewhere, so the npm_config_* / pnpm_config_*
        // keys land in the map as upper-cased under Windows and as
        // lower-cased under Linux/macOS. The test runs on whatever
        // host the suite is invoked on, so we check both casings.
        let upper_optional = output
            .get("NPM_CONFIG_OPTIONAL")
            .or_else(|| output.get("npm_config_optional"))
            .expect("npm_config_optional must be present");
        assert_eq!(upper_optional, &OsStr::new("true"));
        let upper_pnpm_optional = output
            .get("PNPM_CONFIG_OPTIONAL")
            .or_else(|| output.get("pnpm_config_optional"))
            .expect("pnpm_config_optional must be present");
        assert_eq!(upper_pnpm_optional, &OsStr::new("true"));
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
            .inherit("HTTPS_PROXY")
            .replace("NPM_CONFIG_REGISTRY", "https://box.example/")
            .replace("NPM_CONFIG_USERCONFIG", "box-npmrc")
            .apply_to_map(input);
        assert_eq!(
            output.get("NPM_CONFIG_REGISTRY").unwrap(),
            "https://box.example/"
        );
        assert_eq!(output.get("NPM_CONFIG_USERCONFIG").unwrap(), "box-npmrc");
        assert_eq!(output.get("SYSTEMROOT").unwrap(), "system-root");
        // Proxy variables are explicitly inherited so devs behind a
        // corporate mirror can still reach GitHub / the npm registry.
        assert_eq!(output.get("HTTPS_PROXY").unwrap(), "host-proxy");
        assert!(!output.contains_key("APPDATA"));
        assert!(!output.contains_key("LOCALAPPDATA"));
        assert!(!output.contains_key("NODE_PATH"));
    }

    #[test]
    fn dsh_host_policy_strips_proxy_aliases() {
        // The DSH host web server and any node-based client it spawns
        // route `127.0.0.1` fetches through `HTTP_PROXY`/`HTTPS_PROXY`
        // by default. When a developer session has those variables
        // pointing at a sidecar proxy, the proxy answers 502 for the
        // host's own listener and the host self-terminates shortly
        // after printing "opening the default browser". The host policy
        // must strip every proxy alias regardless of whether the parent
        // daemon was a clean-room or a fully-inheriting process.
        let mut input = BTreeMap::new();
        input.insert(
            "HTTP_PROXY".to_owned(),
            OsString::from("http://127.0.0.1:7892"),
        );
        input.insert(
            "HTTPS_PROXY".to_owned(),
            OsString::from("http://127.0.0.1:7892"),
        );
        input.insert(
            "http_proxy".to_owned(),
            OsString::from("http://127.0.0.1:7892"),
        );
        input.insert(
            "https_proxy".to_owned(),
            OsString::from("http://127.0.0.1:7892"),
        );
        input.insert(
            "ALL_PROXY".to_owned(),
            OsString::from("http://127.0.0.1:7892"),
        );
        input.insert(
            "all_proxy".to_owned(),
            OsString::from("http://127.0.0.1:7892"),
        );
        input.insert("NO_PROXY".to_owned(), OsString::from("localhost"));
        input.insert("no_proxy".to_owned(), OsString::from("localhost"));
        // A caller-sandbox internal variable: must not reach the host.
        // An ambient provider key: user intent, passes via the
        // `_API_KEY` suffix rule.
        input.insert(
            "CODEX_SESSION_ID".to_owned(),
            OsString::from("0199-session"),
        );
        input.insert("OPENAI_API_KEY".to_owned(), OsString::from("sk-test"));
        // HOME is on the whitelist and must still flow through.
        input.insert("HOME".to_owned(), OsString::from("/home/wpp"));
        let policy = dsh_host_policy(
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("profile/web/node_modules"),
        );
        let output = policy.apply_to_map(input);
        assert!(
            !output.contains_key("HTTP_PROXY"),
            "HTTP_PROXY leaked into host policy: {output:?}"
        );
        assert!(
            !output.contains_key("HTTPS_PROXY"),
            "HTTPS_PROXY leaked into host policy: {output:?}"
        );
        assert!(
            !output.contains_key("http_proxy"),
            "http_proxy leaked into host policy: {output:?}"
        );
        assert!(
            !output.contains_key("https_proxy"),
            "https_proxy leaked into host policy: {output:?}"
        );
        assert!(
            !output.contains_key("ALL_PROXY"),
            "ALL_PROXY leaked into host policy: {output:?}"
        );
        assert!(
            !output.contains_key("all_proxy"),
            "all_proxy leaked into host policy: {output:?}"
        );
        assert!(
            !output.contains_key("NO_PROXY"),
            "NO_PROXY leaked into host policy: {output:?}"
        );
        assert!(
            !output.contains_key("no_proxy"),
            "no_proxy leaked into host policy: {output:?}"
        );
        // HOME is whitelisted and the provider key passes the
        // `_API_KEY` suffix rule, so both survive. Everything else the
        // caller happened to be running with stays out — sandbox
        // internals in particular.
        assert_eq!(output.get("HOME").unwrap(), "/home/wpp");
        assert!(
            !output.contains_key("CODEX_SESSION_ID"),
            "caller sandbox variable leaked into host policy: {output:?}"
        );
        assert_eq!(output.get("OPENAI_API_KEY").unwrap(), "sk-test");
    }

    #[test]
    fn dsh_host_policy_whitelists_instead_of_passing_through() {
        // The host runs as a clean room: only whitelisted variables
        // survive, PATH is appended after the bundled runtime, and the
        // host-owned values still land. A daemon started from a polluted
        // shell or an agent sandbox must not be able to hand its whole
        // environment to every container it starts.
        let mut input = BTreeMap::new();
        input.insert("LANG".to_owned(), OsString::from("zh_CN.UTF-8"));
        input.insert("LC_ALL".to_owned(), OsString::from("en_US.UTF-8"));
        input.insert("LC_CTYPE".to_owned(), OsString::from("en_US.UTF-8"));
        input.insert("TZ".to_owned(), OsString::from("Asia/Shanghai"));
        input.insert("HOME".to_owned(), OsString::from("/home/wpp"));
        input.insert("USER".to_owned(), OsString::from("wpp"));
        input.insert("TERM".to_owned(), OsString::from("xterm-256color"));
        input.insert(
            "SSH_AUTH_SOCK".to_owned(),
            OsString::from("/run/user/1000/keyring/ssh"),
        );
        input.insert(
            "XDG_RUNTIME_DIR".to_owned(),
            OsString::from("/run/user/1000"),
        );
        input.insert(
            "PATH".to_owned(),
            env::join_paths([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]).unwrap(),
        );
        // Caller-side state that must not cross into the container.
        input.insert(
            "CODEX_SESSION_ID".to_owned(),
            OsString::from("0199-session"),
        );
        input.insert(
            "CODEX_APP_TOOLS_PIPE_PATH".to_owned(),
            OsString::from("/tmp/codex-tools.sock"),
        );
        input.insert(
            "CODEX_PERMISSION_PROFILE".to_owned(),
            OsString::from("sandbox"),
        );
        input.insert("OPENAI_API_KEY".to_owned(), OsString::from("sk-test"));
        input.insert("GITHUB_ACCESS_TOKEN".to_owned(), OsString::from("gh-token"));
        input.insert("SHELL".to_owned(), OsString::from("/usr/bin/zsh"));
        input.insert("COLORTERM".to_owned(), OsString::from("truecolor"));

        let policy = dsh_host_policy(
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("profile/web/node_modules"),
        );
        let output = policy.apply_to_map(input);

        // Whitelisted variables survive unchanged.
        for key in [
            "LANG",
            "LC_ALL",
            "LC_CTYPE",
            "TZ",
            "HOME",
            "USER",
            "TERM",
            "SSH_AUTH_SOCK",
            "XDG_RUNTIME_DIR",
        ] {
            assert!(
                output.contains_key(key),
                "{key} dropped from host policy: {output:?}"
            );
        }
        assert_eq!(output["LANG"], OsString::from("zh_CN.UTF-8"));
        // Provider credentials are user intent: the `_API_KEY` family
        // passes; a token-named variable does not.
        assert_eq!(output.get("OPENAI_API_KEY").unwrap(), "sk-test");
        assert!(!output.contains_key("GITHUB_ACCESS_TOKEN"));

        // Caller sandbox internals and unrelated ambient state stay out;
        // the `_API_KEY` family passes (asserted below).
        for key in [
            "CODEX_SESSION_ID",
            "CODEX_APP_TOOLS_PIPE_PATH",
            "CODEX_PERMISSION_PROFILE",
            "GITHUB_ACCESS_TOKEN",
            "SHELL",
            "COLORTERM",
        ] {
            assert!(
                !output.contains_key(key),
                "{key} leaked into host policy: {output:?}"
            );
        }

        // PATH keeps the host filesystem's binaries after the bundled
        // runtime — a container shares the host filesystem, so the shell
        // it serves needs /usr/bin and friends.
        let paths: Vec<PathBuf> = env::split_paths(&output["PATH"]).collect();
        assert_eq!(paths[0], PathBuf::from("bundled/node"));
        assert_eq!(paths[1], PathBuf::from("bundled/pnpm"));
        assert!(paths.contains(&PathBuf::from("/usr/bin")));
        assert!(paths.contains(&PathBuf::from("/bin")));

        // Host-owned values are still placed.
        assert_eq!(
            output["NODE_PATH"].as_os_str(),
            Path::new("profile/web/node_modules").as_os_str()
        );
        assert_eq!(output["CHOKIDAR_USEPOLLING"], OsString::from("true"));
    }

    #[test]
    fn dsh_host_policy_drops_c_family_lc_all_shadowing_a_real_utf8_lang() {
        // uutils coreutils (Ubuntu 25.10's /bin/ls) resolves the
        // encoding from the locale *name* and treats the whole C/POSIX
        // family — C.UTF-8 included — as ASCII. A `C.UTF-8` `LC_ALL`
        // shadowing `LANG=zh_CN.UTF-8` therefore escapes every
        // non-ASCII filename on the terminal, so the host policy drops
        // the C-family override when LANG names a real UTF-8 locale.
        let mut input = BTreeMap::new();
        input.insert("LANG".to_owned(), OsString::from("zh_CN.UTF-8"));
        input.insert("LC_ALL".to_owned(), OsString::from("C.UTF-8"));
        input.insert("LC_CTYPE".to_owned(), OsString::from("C.UTF-8"));
        input.insert("TZ".to_owned(), OsString::from("Asia/Shanghai"));
        let policy = dsh_host_policy(
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("profile/web/node_modules"),
        );
        let output = policy.apply_to_map(input);
        assert_eq!(output["LANG"], OsString::from("zh_CN.UTF-8"));
        assert!(
            !output.contains_key("LC_ALL"),
            "C-family LC_ALL survived: {output:?}"
        );
        assert!(
            !output.contains_key("LC_CTYPE"),
            "C-family LC_CTYPE survived: {output:?}"
        );
        assert_eq!(output["TZ"], OsString::from("Asia/Shanghai"));
    }

    #[test]
    fn dsh_host_policy_keeps_c_family_lc_all_without_a_real_utf8_lang() {
        // Without a real UTF-8 LANG the C-family values are the only
        // locale signal the child has, so they are kept as-is.
        for lang in [None, Some("C"), Some("POSIX")] {
            let mut input = BTreeMap::new();
            if let Some(lang) = lang {
                input.insert("LANG".to_owned(), OsString::from(lang));
            }
            input.insert("LC_ALL".to_owned(), OsString::from("C.UTF-8"));
            input.insert("LC_CTYPE".to_owned(), OsString::from("C.UTF-8"));
            let policy = dsh_host_policy(
                Path::new("bundled/node"),
                Path::new("bundled/pnpm"),
                Path::new("profile/web/node_modules"),
            );
            let output = policy.apply_to_map(input);
            assert_eq!(
                output.get("LC_ALL").map(OsString::as_os_str),
                Some(OsStr::new("C.UTF-8")),
                "LC_ALL dropped for LANG={lang:?}"
            );
            assert_eq!(
                output.get("LC_CTYPE").map(OsString::as_os_str),
                Some(OsStr::new("C.UTF-8")),
                "LC_CTYPE dropped for LANG={lang:?}"
            );
        }
    }

    #[test]
    fn inherit_suffix_passes_only_the_named_family() {
        let mut input = BTreeMap::new();
        input.insert("OPENAI_API_KEY".to_owned(), OsString::from("sk-openai"));
        input.insert("STEP_API_KEY".to_owned(), OsString::from("sk-step"));
        input.insert(
            "CODEX_SESSION_ID".to_owned(),
            OsString::from("0199-session"),
        );
        input.insert("GITHUB_ACCESS_TOKEN".to_owned(), OsString::from("gh-token"));
        input.insert("UNRELATED".to_owned(), OsString::from("yes"));
        let output = EnvironmentPolicy::new()
            .clean_room()
            .inherit_suffix("_API_KEY")
            .apply_to_map(input);
        assert_eq!(output.get("OPENAI_API_KEY").unwrap(), "sk-openai");
        assert_eq!(output.get("STEP_API_KEY").unwrap(), "sk-step");
        assert!(!output.contains_key("CODEX_SESSION_ID"));
        assert!(!output.contains_key("GITHUB_ACCESS_TOKEN"));
        assert!(!output.contains_key("UNRELATED"));
    }

    #[test]
    fn declared_env_replaces_whitelisted_values_but_not_task_overrides() {
        // A container's declared ENV wins over the whitelisted parent
        // value, and Box's own task_override still wins over both.
        let mut declared = BTreeMap::new();
        declared.insert("TZ".to_owned(), "Asia/Tokyo".to_owned());
        declared.insert("DSH_HOME".to_owned(), "/declared/home".to_owned());
        let mut input = BTreeMap::new();
        input.insert("TZ".to_owned(), OsString::from("Asia/Shanghai"));
        let output = EnvironmentPolicy::new()
            .clean_room()
            .inherit("TZ")
            .declared_env(&declared)
            .task_override("DSH_HOME", "/box/home")
            .apply_to_map(input);
        assert_eq!(output["TZ"], OsString::from("Asia/Tokyo"));
        assert_eq!(output["DSH_HOME"], OsString::from("/box/home"));
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
        input.insert(
            "PATH".to_owned(),
            env::join_paths([PathBuf::from("host-path")]).unwrap(),
        );
        input.insert("HOME".to_owned(), OsString::from("C:\\Users\\host"));
        input.insert(
            "GIT_CONFIG_GLOBAL".to_owned(),
            OsString::from("C:\\Users\\host\\.gitconfig"),
        );
        input.insert(
            "HTTPS_PROXY".to_owned(),
            OsString::from("http://host-proxy:8080"),
        );

        let policy = bundled_package_manager_policy(
            None,
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("box-data"),
            Some("https://registry.npmjs.org/"),
            Some(Path::new("bundled/git/cmd")),
            None,
            true,
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
        let expected_home = PathBuf::from("box-data").join("git").join("home");
        assert_eq!(output["HOME"].as_os_str(), expected_home.as_os_str());
        let expected_global = PathBuf::from("box-data")
            .join("git")
            .join("config")
            .join("global.gitconfig");
        assert_eq!(
            output["GIT_CONFIG_GLOBAL"].as_os_str(),
            expected_global.as_os_str()
        );
        assert_eq!(output["GIT_CONFIG_NOSYSTEM"], OsString::from("1"));
        assert_eq!(output["GIT_TERMINAL_PROMPT"], OsString::from("0"));

        use std::ffi::OsStr;
        // Proxy variables are inherited so devs behind a corporate
        // mirror can still reach GitHub and the npm registry.
        assert_eq!(
            output.get("HTTPS_PROXY").map(OsString::as_os_str),
            Some(OsStr::new("http://host-proxy:8080"))
        );
    }

    /// The whole point of `inherit_proxy`: the flag decides, and neither
    /// casing of any alias survives a hermetic child.
    #[test]
    fn bundled_package_manager_policy_gates_every_proxy_alias_on_the_flag() {
        const ALIASES: [&str; 8] = [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "https_proxy",
            "http_proxy",
            "ALL_PROXY",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        ];
        let mut input = BTreeMap::new();
        input.insert(
            "PATH".to_owned(),
            env::join_paths([PathBuf::from("host-path")]).unwrap(),
        );
        for alias in ALIASES {
            input.insert(alias.to_owned(), OsString::from("http://host-proxy:8080"));
        }
        let build = |inherit: bool| {
            bundled_package_manager_policy(
                None,
                Path::new("bundled/node"),
                Path::new("bundled/pnpm"),
                Path::new("box-data"),
                Some("https://registry.npmjs.org/"),
                Some(Path::new("bundled/git/cmd")),
                None,
                inherit,
            )
            .unwrap()
            .apply_to_map(input.clone())
        };

        let hermetic = build(false);
        for alias in ALIASES {
            assert!(
                !hermetic.contains_key(alias),
                "{alias} leaked into a hermetic child: {hermetic:?}"
            );
        }
        let inherited = build(true);
        for alias in ALIASES {
            assert_eq!(
                inherited.get(alias).map(OsString::as_os_str),
                Some(OsStr::new("http://host-proxy:8080")),
                "{alias} must survive when the host proxy is inherited"
            );
        }
    }

    #[test]
    fn bundled_package_manager_policy_prepends_host_git_and_isolates_config() {
        // Seed the clean-room input with a host-shaped PATH, HOME, and
        // GIT_CONFIG_GLOBAL. After applying the policy with no bundled git
        // but a host fallback, PATH must include the host git directory
        // (first), HOME must point at <storage>/git/home, and host
        // sentinel values must not leak through.
        let mut input = BTreeMap::new();
        input.insert(
            "PATH".to_owned(),
            env::join_paths([PathBuf::from("/usr/bin"), PathBuf::from("/bin")]).unwrap(),
        );
        input.insert("HOME".to_owned(), OsString::from("/home/host"));
        input.insert(
            "GIT_CONFIG_GLOBAL".to_owned(),
            OsString::from("/home/host/.gitconfig"),
        );
        input.insert(
            "XDG_CONFIG_HOME".to_owned(),
            OsString::from("/home/host/.config"),
        );
        input.insert(
            "HTTPS_PROXY".to_owned(),
            OsString::from("http://host-proxy:8080"),
        );

        let policy = bundled_package_manager_policy(
            None,
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("box-data"),
            Some("https://registry.npmjs.org/"),
            None,
            Some(Path::new("/usr/bin")),
            true,
        )
        .unwrap();
        let output = policy.apply_to_map(input);

        // /usr/bin (host git) is prepended to PATH after node/pnpm.
        let paths: Vec<PathBuf> = env::split_paths(&output["PATH"]).collect();
        assert_eq!(paths[0], PathBuf::from("bundled/node"));
        assert_eq!(paths[1], PathBuf::from("bundled/pnpm"));
        assert_eq!(paths[2], PathBuf::from("/usr/bin"));
        assert!(!paths.contains(&PathBuf::from("/bin")));

        // Host HOME is replaced by the managed git HOME.
        let expected_home = PathBuf::from("box-data").join("git").join("home");
        assert_eq!(output["HOME"].as_os_str(), expected_home.as_os_str());

        // GIT_CONFIG_GLOBAL points at the Box-owned empty file, not the
        // host ~/.gitconfig. The parent of that file is also the XDG
        // root, so XDG_CONFIG_HOME is set to the same directory.
        let expected_global = PathBuf::from("box-data")
            .join("git")
            .join("config")
            .join("global.gitconfig");
        assert_eq!(
            output["GIT_CONFIG_GLOBAL"].as_os_str(),
            expected_global.as_os_str()
        );
        assert_eq!(output["GIT_CONFIG_NOSYSTEM"], OsString::from("1"));
        assert_eq!(output["GIT_TERMINAL_PROMPT"], OsString::from("0"));
        assert_eq!(
            output["XDG_CONFIG_HOME"].as_os_str(),
            expected_global.parent().unwrap().as_os_str(),
        );

        use std::ffi::OsStr;
        // Proxy variables are inherited so devs behind a corporate
        // mirror can still reach GitHub and the npm registry.
        assert_eq!(
            output.get("HTTPS_PROXY").map(OsString::as_os_str),
            Some(OsStr::new("http://host-proxy:8080"))
        );

        // Host git never sees LD_LIBRARY_PATH — the system loader resolves
        // its .so dependencies. This assertion is unconditional; the bundled
        // branch is the only path that emits LD_LIBRARY_PATH.
        assert!(!output.contains_key("LD_LIBRARY_PATH"));
    }

    #[test]
    fn bundled_package_manager_policy_errors_when_neither_bundled_nor_host_git_available() {
        let result = bundled_package_manager_policy(
            None,
            Path::new("bundled/node"),
            Path::new("bundled/pnpm"),
            Path::new("box-data"),
            Some("https://registry.npmjs.org/"),
            None,
            None,
            true,
        );
        let error = result.expect_err("must error when neither bundled nor host git is available");
        assert!(
            error.contains("git"),
            "error message should mention git: {error}"
        );
    }
