//! Integration coverage for the real `EnvironmentPolicy::apply` path the
//! daemon uses to spawn a container's DSH host. The unit tests in
//! `process::env` cover the policy table through `apply_to_map`; this test
//! spawns an actual child (`/usr/bin/env`) with the policy applied, so the
//! environment the host process would really receive is what gets asserted.

use box_runtime::process::dsh_host_policy;
use std::{collections::BTreeMap, env, ffi::OsString, path::Path, process::Command};

/// Variables the test seeds in its own process to imitate a daemon that
/// was started from an agent sandbox, and restores afterwards.
const SEEDED: &[&str] = &[
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "HOME",
    "SSH_AUTH_SOCK",
    "CODEX_SESSION_ID",
    "CODEX_APP_TOOLS_PIPE_PATH",
    "CODEX_PERMISSION_PROFILE",
    "OPENAI_API_KEY",
    "SHELL",
];

fn spawn_with_host_policy() -> String {
    // Values the whitelist must pass through (with the C-family LC_ALL /
    // LC_CTYPE shadowing a real UTF-8 LANG, which must be dropped).
    env::set_var("LANG", "zh_CN.UTF-8");
    env::set_var("LC_ALL", "C.UTF-8");
    env::set_var("LC_CTYPE", "C.UTF-8");
    env::set_var("TZ", "Asia/Shanghai");
    env::set_var("HOME", "/home/wpp");
    env::set_var("SSH_AUTH_SOCK", "/run/user/1000/keyring/ssh");
    // Caller-side state that must not cross into the container.
    env::set_var("CODEX_SESSION_ID", "0199-test-session");
    env::set_var("CODEX_APP_TOOLS_PIPE_PATH", "/tmp/codex-tools.sock");
    env::set_var("CODEX_PERMISSION_PROFILE", "sandbox");
    env::set_var("GITHUB_ACCESS_TOKEN", "gh-token");
    env::set_var("SHELL", "/usr/bin/zsh");
    // A provider key is user intent, not sandbox state: it passes.
    env::set_var("OPENAI_API_KEY", "sk-test");

    let policy = dsh_host_policy(
        Path::new("/opt/dshbox/node"),
        Path::new("/opt/dshbox/pnpm"),
        Path::new("/runtime/profile/web/node_modules"),
    );
    let mut command = Command::new("/usr/bin/env");
    policy.apply(&mut command);
    let output = command.output().expect("/usr/bin/env must run");
    assert!(output.status.success(), "env child failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The composition the daemon performs at container start: the whitelist
/// policy plus the container's declared ENV (read from `container.json`).
#[cfg(unix)]
#[test]
fn declared_env_lands_in_the_spawned_host_child() {
    let saved: Vec<(String, Option<OsString>)> = SEEDED
        .iter()
        .map(|key| ((*key).to_owned(), env::var_os(key)))
        .collect();
    env::set_var("LANG", "zh_CN.UTF-8");
    env::set_var("TZ", "Asia/Shanghai");

    let mut declared = BTreeMap::new();
    declared.insert("TZ".to_owned(), "Asia/Tokyo".to_owned());
    declared.insert("DSH_HOME".to_owned(), "/declared/home".to_owned());

    let policy = dsh_host_policy(
        Path::new("/opt/dshbox/node"),
        Path::new("/opt/dshbox/pnpm"),
        Path::new("/runtime/profile/web/node_modules"),
    )
    .declared_env(&declared)
    .task_override("DSH_HOME", "/box/home");
    let mut command = Command::new("/usr/bin/env");
    policy.apply(&mut command);
    let output = command.output().expect("/usr/bin/env must run");
    assert!(output.status.success(), "env child failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    // Declared values replace the whitelisted parent value ...
    assert!(
        lines.contains(&"TZ=Asia/Tokyo"),
        "declared TZ missing:\n{stdout}"
    );
    // ... but Box's own task_override still wins over a declaration that
    // names the same variable.
    assert!(
        lines.contains(&"DSH_HOME=/box/home"),
        "DSH_HOME wrong:\n{stdout}"
    );

    for (key, value) in saved {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }
}

#[cfg(unix)]
#[test]
fn spawned_host_child_gets_the_whitelist_not_the_parent_environment() {
    let saved: Vec<(String, Option<OsString>)> = SEEDED
        .iter()
        .map(|key| ((*key).to_owned(), env::var_os(key)))
        .collect();
    let stdout = spawn_with_host_policy();
    let lines: Vec<&str> = stdout.lines().collect();
    let has = |key: &str| {
        lines
            .iter()
            .any(|line| line.starts_with(&format!("{key}=")))
    };

    // Whitelisted variables reach the child.
    for key in ["LANG", "TZ", "HOME", "SSH_AUTH_SOCK", "PATH"] {
        assert!(has(key), "{key} missing from host child env:\n{stdout}");
    }
    assert!(
        lines.contains(&"LANG=zh_CN.UTF-8"),
        "LANG value wrong:\n{stdout}"
    );

    // A C-family LC_ALL/LC_CTYPE shadowing a real UTF-8 LANG is dropped:
    // uutils coreutils reads the locale *name* and treats C.UTF-8 as ASCII.
    assert!(!has("LC_ALL"), "C-family LC_ALL leaked:\n{stdout}");
    assert!(!has("LC_CTYPE"), "C-family LC_CTYPE leaked:\n{stdout}");

    // Caller sandbox internals and unrelated ambient state stay out; the
    // `_API_KEY` family passes (asserted below).
    for key in [
        "CODEX_SESSION_ID",
        "CODEX_APP_TOOLS_PIPE_PATH",
        "CODEX_PERMISSION_PROFILE",
        "GITHUB_ACCESS_TOKEN",
        "SHELL",
    ] {
        assert!(!has(key), "{key} leaked into host child env:\n{stdout}");
    }
    assert!(
        lines.contains(&"OPENAI_API_KEY=sk-test"),
        "provider key missing from host child env:\n{stdout}"
    );

    // Proxy aliases stay out (loopback poison).
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "http_proxy",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        assert!(!has(key), "{key} leaked into host child env:\n{stdout}");
    }

    // PATH is prepended with the bundled runtime and still carries the
    // system directories (a container shares the host filesystem).
    let path = lines
        .iter()
        .find(|line| line.starts_with("PATH="))
        .map(|line| line.trim_start_matches("PATH="))
        .unwrap_or_default();
    assert!(
        path.starts_with("/opt/dshbox/node"),
        "bundled node not first on PATH: {path}"
    );
    assert!(path.contains("/usr/bin"), "system PATH lost: {path}");

    // Host-owned values land.
    assert!(has("NODE_PATH"), "NODE_PATH missing:\n{stdout}");
    assert!(
        has("CHOKIDAR_USEPOLLING"),
        "CHOKIDAR_USEPOLLING missing:\n{stdout}"
    );

    // Restore the parent environment for anything that runs after us.
    for (key, value) in saved {
        match value {
            Some(value) => env::set_var(key, value),
            None => env::remove_var(key),
        }
    }
}
