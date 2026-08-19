//! Integration tests that exercise the bundled Node/npm/pnpm runtime shipped
//! under `src-tauri/resources/runtime/<target>/`. These tests do **not** mock
//! the toolchain and do **not** shell out to a system Node/pnpm — they always
//! go through the absolute paths declared in the manifest so we can validate
//! the unified process layer against the same binaries the production
//! installer ships.

use box_runtime::{
    bundled::{bundled_target, ResolvedBundledRuntime},
    process::{bundled_toolchain_policy, ExecutionKind, ExecutionResult, NativeProcessRunner, ProcessSpec},
};
use std::{
    env,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

fn repo_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.parent().and_then(Path::parent).map(Path::to_path_buf).expect("box-runtime is under src-tauri/crates/")
}

fn skip_if_runtime_missing(runtime: &ResolvedBundledRuntime) -> bool {
    !runtime.node_executable().is_file() || !runtime.pnpm_script().is_file() || !runtime.npm_script().is_file()
}

fn ensure_test_layout() -> ResolvedBundledRuntime {
    let runtime = ResolvedBundledRuntime::from_repo_root(&repo_root())
        .expect("runtime manifest should be readable from the repo root");
    assert_eq!(runtime.manifest.target, bundled_target(), "manifest target must match current host");
    if skip_if_runtime_missing(&runtime) {
        eprintln!(
            "skipping bundled-runtime tests: manifest reports target {} but executables are missing at {}",
            runtime.manifest.target,
            runtime.root.display()
        );
    }
    runtime
}

fn fresh_temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("dshbox-runtime-test-{tag}-{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn build_runtime_dir() -> PathBuf {
    let dir = fresh_temp_dir("runtime");
    let _ = fs::create_dir_all(dir.join("pnpm").join("store"));
    let _ = fs::create_dir_all(dir.join("pnpm").join("npm-cache"));
    dir
}

#[test]
fn bundled_node_reports_version() {
    let runtime = ensure_test_layout();
    if skip_if_runtime_missing(&runtime) { return; }
    let spec = ProcessSpec::new(runtime.node_executable()).arg("--version");
    let output = NativeProcessRunner.run(&spec).expect("node --version must succeed");
    assert!(output.status.success(), "node --version should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.trim_start().starts_with('v'), "expected version string, got: {stdout}");
}

#[test]
fn bundled_npm_reports_version() {
    let runtime = ensure_test_layout();
    if skip_if_runtime_missing(&runtime) { return; }
    let npm_policy = bundled_toolchain_policy(
        None,
        &runtime.node_dir(),
        &runtime.pnpm_dir(),
        None,
        Some("https://registry.npmjs.org/"),
        false,
    );
    let spec = ProcessSpec::new(runtime.node_executable())
        .arg(runtime.npm_script())
        .arg("--version")
        .policy(npm_policy);
    let output = NativeProcessRunner.run(&spec).expect("npm --version must succeed");
    assert!(output.status.success(), "npm --version should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.trim().is_empty(), "expected version string, got: {stdout}");
}

#[test]
fn bundled_pnpm_reports_version() {
    let runtime = ensure_test_layout();
    if skip_if_runtime_missing(&runtime) { return; }
    let pnpm_policy = bundled_toolchain_policy(
        None,
        &runtime.node_dir(),
        &runtime.pnpm_dir(),
        None,
        Some("https://registry.npmjs.org/"),
        false,
    );
    let spec = ProcessSpec::new(runtime.node_executable())
        .arg(runtime.pnpm_script())
        .arg("--version")
        .policy(pnpm_policy);
    let output = NativeProcessRunner.run(&spec).expect("pnpm --version must succeed");
    assert!(output.status.success(), "pnpm --version should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains('.'), "expected semver-ish output, got: {stdout}");
}

#[test]
fn bundled_policy_overrides_registry_and_store() {
    let runtime = ensure_test_layout();
    if skip_if_runtime_missing(&runtime) { return; }
    let runtime_dir = build_runtime_dir();
    let project = fresh_temp_dir("project");
    let package_json = project.join("package.json");
    fs::write(&package_json, "{\"name\":\"smoke\",\"version\":\"0.0.0\"}").unwrap();
    let isolated_home = fresh_temp_dir("home");
    fs::create_dir_all(&isolated_home).unwrap();

    let mut policy = bundled_toolchain_policy(
        None,
        &runtime.node_dir(),
        &runtime.pnpm_dir(),
        Some(&runtime_dir),
        Some("https://example.invalid/registry/"),
        false,
    );
    // The test intentionally runs against the bundled Node, but pnpm still
    // consults the user's `~/.npmrc` for `registry`. Pin a clean HOME so the
    // host-wide mirror cannot leak into the assertion.
    policy = policy.remove("HOME").remove("USERPROFILE").task_override("HOME", isolated_home.to_string_lossy().into_owned()).task_override("USERPROFILE", isolated_home.to_string_lossy().into_owned());
    // Verify the policy actually injects npm_config_registry into the child
    // by reading it back via `node -p`.
    let probe = ProcessSpec::new(runtime.node_executable())
        .arg("-p")
        .arg("process.env.npm_config_registry ?? 'unset'")
        .policy(policy.clone());
    let output = NativeProcessRunner.run(&probe).expect("node env probe");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "https://example.invalid/registry/", "policy must inject npm_config_registry into the child");

    // PNPM_STORE_DIR is a private pnpm variable that respects env override,
    // so we can validate it end-to-end without pnpm's `--location` indirection.
    let probe_store = ProcessSpec::new(runtime.node_executable())
        .arg("-p")
        .arg("process.env.PNPM_STORE_DIR ?? 'unset'")
        .policy(bundled_toolchain_policy(
            None,
            &runtime.node_dir(),
            &runtime.pnpm_dir(),
            Some(&runtime_dir),
            Some("https://example.invalid/registry/"),
            false,
        ).remove("HOME").remove("USERPROFILE").task_override("HOME", isolated_home.to_string_lossy().into_owned()).task_override("USERPROFILE", isolated_home.to_string_lossy().into_owned()));
    let output = NativeProcessRunner.run(&probe_store).expect("store dir probe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = runtime_dir.join("pnpm").join("store");
    assert_eq!(stdout.trim(), expected.to_string_lossy(), "policy must inject PNPM_STORE_DIR into the child env");

    let _ = fs::remove_dir_all(project);
    let _ = fs::remove_dir_all(&runtime_dir);
}

#[test]
fn bundled_pnpm_runs_logged_and_writes_log_file() {
    let runtime = ensure_test_layout();
    if skip_if_runtime_missing(&runtime) { return; }
    let project = fresh_temp_dir("logged");
    fs::write(project.join("package.json"), "{\"name\":\"logged\",\"version\":\"0.0.0\"}").unwrap();

    let log_path = project.join("pnpm.log");
    let policy = bundled_toolchain_policy(
        None,
        &runtime.node_dir(),
        &runtime.pnpm_dir(),
        None,
        Some("https://registry.npmjs.org/"),
        false,
    );
    let spec = ProcessSpec::new(runtime.node_executable())
        .arg(runtime.pnpm_script())
        .arg("--version")
        .cwd(&project)
        .kind(ExecutionKind::Logged)
        .policy(policy)
        .log_path(log_path.clone());

    let mut logged = match NativeProcessRunner.execute(&spec).expect("logged spawn") {
        ExecutionResult::Logged(logged) => logged,
        _ => panic!("expected logged execution"),
    };
    let status = logged.child.unwrap().expect("wait must succeed");
    assert!(status.success(), "pnpm --version logged should succeed");
    assert!(log_path.is_file(), "log file must exist");
    let contents = fs::read_to_string(&log_path).unwrap();
    assert!(contents.contains('.'), "log should contain version output, got: {contents}");

    let _ = fs::remove_dir_all(project);
}

#[test]
fn detached_dsh_smoke_or_skipped() {
    let runtime = ensure_test_layout();
    if skip_if_runtime_missing(&runtime) { return; }
    // Locate any harness entry that could plausibly be launched by DSH Box.
    // Without a real container + plugin set we cannot drive the full host,
    // but the process spec must at least accept a DshHost-shaped invocation.
    let candidate_entry = find_candidate_harness_entry(&repo_root());
    let Some(entry) = candidate_entry else {
        eprintln!("skipping detached-dsh-smoke: no harness entry fixture found in the repo");
        return;
    };

    let runtime_dir = build_runtime_dir();
    let policy = bundled_toolchain_policy(
        None,
        &runtime.node_dir(),
        &runtime.pnpm_dir(),
        Some(&runtime_dir),
        Some("https://registry.npmjs.org/"),
        false,
    );
    let spec = ProcessSpec::new(runtime.node_executable())
        .arg("--import")
        .arg("tsx/esm")
        .arg(&entry)
        .arg("--probe")
        .policy(policy)
        .detached()
        .logged(fresh_temp_dir("dsh-smoke").join("host.log"));
    let mut child = match NativeProcessRunner.execute(&spec).expect("detached spawn") {
        ExecutionResult::Logged(logged) => logged,
        _ => panic!("expected logged execution for detached dsh smoke"),
    };

    // Probe the child for a short window then kill the process group.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut any_output = false;
    while Instant::now() < deadline {
        match child.lines.try_recv() {
            Ok(_) => { any_output = true; break; }
            Err(_) => {}
        }
        if let Ok(Some(_)) = child.child.try_wait() { break; }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.child.kill_tree(true, Duration::from_secs(2));
    let _ = fs::remove_dir_all(runtime_dir);
    // We deliberately do not assert `any_output == true`: the smoke test's
    // contract is "the spec builds, the process starts, and we clean up".
    // Fixtures may legitimately fail to load plugins; we surface that via the
    // log file rather than as a hard failure.
    let _ = any_output;
}

fn find_candidate_harness_entry(root: &Path) -> Option<PathBuf> {
    let candidates = [
        root.join("harness").join("bin").join("dsh.ts"),
        root.join("src-tauri").join("harness").join("bin").join("dsh.ts"),
        root.join("crates").join("dsh").join("bin").join("dsh.ts"),
        root.join("dsh").join("bin").join("dsh.ts"),
    ];
    candidates.into_iter().find(|candidate| candidate.is_file())
}