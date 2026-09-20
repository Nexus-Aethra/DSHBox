use crate::kinds::{Conflict, Shape};
use crate::{discover, transfer};
use std::fs;
use std::path::{Path, PathBuf};

fn temp_dir(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("box-resources-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn fixture_container(name: &str) -> PathBuf {
    let root = temp_dir(name);
    write(&root.join("profile/sessions/--home-wpp--/session-1/session.v3.jsonl.zstd"), "chat-1");
    write(&root.join("profile/sessions/--home-wpp--/session-2/session.v3.jsonl.zstd"), "chat-2");
    write(&root.join("profile/.credentials.yaml"), "version: 1\nrefs:\n  A: one\n");
    root
}

#[test]
fn builtin_kinds_cover_the_two_v1_resources() {
    let sessions = crate::builtin("sessions").expect("sessions kind");
    assert_eq!(sessions.path, "profile/sessions");
    assert_eq!(sessions.shape, Shape::Entries);
    assert!(!sessions.secret);

    let credentials = crate::builtin("credentials").expect("credentials kind");
    assert_eq!(credentials.path, "profile/.credentials.yaml");
    assert!(credentials.secret, "credentials are a secret kind");
    assert!(crate::builtin("sshkey").is_none());
}

#[test]
fn package_json_declarations_are_parsed_in_both_shapes() {
    let array = r#"{"name":"@nexus-aethra/dshell-ssh","dshbox":{"resources":[
        {"id":"ssh-keys","path":"dshell/ssh","secret":true,"shape":"entries","label":"SSH keys"}]}}"#;
    let parsed = discover::parse_declarations(array).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].path, "dshell/ssh");
    assert!(parsed[0].secret);

    let map = r#"{"name":"x","dshbox":{"resources":{"browser":{"path":"dshell/browser"}}}}"#;
    let parsed = discover::parse_declarations(map).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].id, "browser");
    assert_eq!(parsed[0].path, "dshell/browser");

    // A package that declares nothing is not an error.
    assert!(discover::parse_declarations(r#"{"name":"plain"}"#).unwrap().is_empty());
    // An entry without a path is.
    let broken = r#"{"dshbox":{"resources":[{"id":"x"}]}}"#;
    assert!(discover::parse_declarations(broken).is_err());
}

#[test]
fn scanned_candidates_read_join_chains_and_stop_at_templates() {
    let source = r#"
        // join(home, 'commented-out') is not code
        const sessions = join(home, "storages", "session_projcache", "sessions", `${id}.json`);
        const other = path.join(profileDir, 'dshell', 'ssh');
        const absolute = join('/etc', 'passwd');
        const computed = join(home, name, 'sessions');
    "#;
    let candidates = discover::scanned_candidates(source);
    assert!(candidates.contains(&"storages/session_projcache/sessions".to_owned()), "{candidates:?}");
    assert!(candidates.contains(&"dshell/ssh".to_owned()), "{candidates:?}");
    // A literal first argument is already an absolute path, and a computed
    // component is something the scan must not guess at.
    assert!(!candidates.iter().any(|c| c.contains("etc")), "{candidates:?}");
    assert!(!candidates.iter().any(|c| c.contains("passwd")), "{candidates:?}");
    assert!(!candidates.iter().any(|c| c.starts_with("sessions")), "{candidates:?}");
}

#[test]
fn discovery_reports_builtins_with_their_size() {
    let container = fixture_container("discover");
    let found = discover::discover(&container, "web", None);
    let sessions = found.iter().find(|entry| entry.kind.id == "sessions").unwrap();
    assert!(sessions.exists);
    assert_eq!(sessions.files, 2, "two sessions");
    assert_eq!(sessions.bytes, 12);
    let credentials = found.iter().find(|entry| entry.kind.id == "credentials").unwrap();
    assert!(credentials.exists);
    assert_eq!(credentials.files, 1);
}

#[test]
fn discovery_prefers_declarations_over_scanned_paths() {
    let container = temp_dir("declare");
    let package = container.join("profile/profiles/web/node_modules/@nexus-aethra/dshell-ssh");
    write(
        &package.join("package.json"),
        r#"{"name":"@nexus-aethra/dshell-ssh","dshbox":{"resources":[{"id":"ssh-keys","path":"dshell/ssh","shape":"entries"}]}}"#,
    );
    write(&package.join("lib/index.js"), "const p = join(home, 'dshell', 'ssh');");
    write(&container.join("profile/dshell/ssh/known_hosts"), "host");

    let found = discover::discover(&container, "web", Some("@nexus-aethra/dshell-ssh"));
    let declared: Vec<_> = found.iter().filter(|entry| entry.scope == discover::Scope::Declared).collect();
    assert_eq!(declared.len(), 1);
    assert_eq!(declared[0].kind.path, "profile/dshell/ssh", "declarations are DSH-home relative");
    assert!(declared[0].exists);
    assert_eq!(declared[0].plugin.as_deref(), Some("@nexus-aethra/dshell-ssh"));
    assert!(
        found.iter().all(|entry| entry.scope != discover::Scope::Inferred),
        "declarations win"
    );
}

#[test]
fn discovery_scans_a_plugin_that_declares_nothing() {
    let container = temp_dir("infer");
    let package = container.join("profile/profiles/web/node_modules/@nexus-aethra/dshell-workspace");
    write(&package.join("package.json"), r#"{"name":"@nexus-aethra/dshell-workspace"}"#);
    write(
        &package.join("lib/purge.js"),
        "await drop(join(home, 'storages', 'session_projcache', 'sessions', `${id}.json`));",
    );
    // The path only counts once it exists on disk.
    write(&container.join("profile/storages/session_projcache/sessions/a.json"), "{}");

    let found = discover::discover(&container, "web", Some("@nexus-aethra/dshell-workspace"));
    let inferred: Vec<_> = found.iter().filter(|entry| entry.scope == discover::Scope::Inferred).collect();
    assert_eq!(inferred.len(), 1, "{found:?}");
    assert_eq!(inferred[0].kind.path, "profile/storages/session_projcache/sessions");
    assert!(inferred[0].kind.inferred);
    assert!(inferred[0].exists);
    assert_eq!(inferred[0].files, 1);
}

#[test]
fn paths_never_escape_the_container() {
    let root = Path::new("/tmp/container");
    assert!(transfer::safe_join(root, "profile/sessions").is_ok());
    assert!(transfer::safe_join(root, "../outside").is_err());
    assert!(transfer::safe_join(root, "/etc/passwd").is_err());
    assert!(transfer::safe_join(root, "..").is_err());
    assert!(transfer::safe_join(root, "").is_err());
}

#[test]
fn extraction_round_trips_into_the_same_place() {
    let container = fixture_container("roundtrip");
    let payload = temp_dir("roundtrip-payload");
    let extracted = transfer::extract(&container, "profile/sessions", None, &payload, false).unwrap();
    assert_eq!(extracted.files, 2);
    assert_eq!(extracted.bytes, 12);
    assert_eq!(transfer::tree_digest(&payload).unwrap(), extracted.digest);

    // Refusing is the default: the destination is not empty.
    let refused = transfer::inject(
        &payload, &container, "profile/sessions", Conflict::Refuse, Shape::Entries, 2, false,
    );
    assert!(refused.is_err(), "refusing is the default");
    assert!(
        container.join("profile/sessions/--home-wpp--/session-1").is_dir(),
        "a refused injection writes nothing"
    );

    // Merging compares sessions, not workspaces: session-1 stays untouched and
    // only the missing session-2 comes back.
    fs::remove_dir_all(container.join("profile/sessions/--home-wpp--/session-2")).unwrap();
    write(&container.join("profile/sessions/--home-wpp--/session-2/session.v3.jsonl.zstd"), "local");
    let merged = transfer::inject(
        &payload, &container, "profile/sessions", Conflict::Merge, Shape::Entries, 2, false,
    )
    .unwrap();
    assert!(merged.replaced.iter().any(|entry| entry == "--home-wpp--/session-2"));
    assert_eq!(
        fs::read_to_string(container.join("profile/sessions/--home-wpp--/session-2/session.v3.jsonl.zstd")).unwrap(),
        "chat-2",
        "the payload wins on a merge"
    );

    // Overwriting empties the kind's path first, so an entry the payload does
    // not carry is gone.
    write(&container.join("profile/sessions/stale/session.v3.jsonl.zstd"), "old");
    let overwritten = transfer::inject(
        &payload, &container, "profile/sessions", Conflict::Overwrite, Shape::Entries, 2, false,
    )
    .unwrap();
    assert!(overwritten.added.iter().any(|entry| entry == "--home-wpp--/session-1"));
    assert!(!container.join("profile/sessions/stale").exists());
}

#[test]
fn credentials_merge_keeps_the_keys_that_are_already_there() {
    let container = temp_dir("credentials");
    let target = container.join("profile/.credentials.yaml");
    write(&target, "version: 1\nrecords:\n  a: keep\nrefs:\n  A: one\n");
    let payload = temp_dir("credentials-payload");
    write(
        &payload.join(".credentials.yaml"),
        "version: 1\nrecords:\n  b: new\nrefs:\n  B: two\n  A: replaced\n",
    );

    // Refuse is still the default, even for a mergeable kind.
    assert!(transfer::inject(&payload, &container, "profile/.credentials.yaml", Conflict::Refuse, Shape::Opaque, 1, true).is_err());

    transfer::inject(&payload, &container, "profile/.credentials.yaml", Conflict::Merge, Shape::Opaque, 1, true).unwrap();
    let merged = fs::read_to_string(&target).unwrap();
    let value: serde_yaml::Value = serde_yaml::from_str(&merged).unwrap();
    assert_eq!(value["records"]["a"].as_str(), Some("keep"));
    assert_eq!(value["records"]["b"].as_str(), Some("new"));
    assert_eq!(value["refs"]["B"].as_str(), Some("two"));
    // The incoming value wins on a conflict, which is what "inject" means.
    assert_eq!(value["refs"]["A"].as_str(), Some("replaced"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a secret kind is tightened to 0600");
    }
}

#[test]
fn opaque_injection_refuses_and_overwrites_by_policy() {
    let container = temp_dir("opaque");
    write(&container.join("profile/.credentials.yaml"), "version: 1\n");
    let payload = temp_dir("opaque-payload");
    write(&payload.join(".credentials.yaml"), "version: 2\n");

    assert!(transfer::inject(&payload, &container, "profile/.credentials.yaml", Conflict::Refuse, Shape::Opaque, 1, false).is_err());
    transfer::inject(&payload, &container, "profile/.credentials.yaml", Conflict::Overwrite, Shape::Opaque, 1, false).unwrap();
    assert_eq!(fs::read_to_string(container.join("profile/.credentials.yaml")).unwrap(), "version: 2\n");
}

#[test]
fn a_tar_round_trip_rebuilds_the_payload() {
    let container = fixture_container("tar");
    let payload = temp_dir("tar-payload");
    transfer::extract(&container, "profile/sessions", None, &payload, false).unwrap();
    let archive = std::env::temp_dir().join(format!("box-resources-{}.tar.gz", std::process::id()));
    transfer::pack_tar(&payload, &archive).unwrap();

    let restored = temp_dir("tar-restored");
    transfer::unpack_tar(&archive, &restored).unwrap();
    assert_eq!(
        transfer::tree_digest(&payload).unwrap(),
        transfer::tree_digest(&restored).unwrap()
    );
    let _ = fs::remove_file(&archive);
}

#[test]
fn resource_ids_are_filesystem_safe() {
    assert_eq!(crate::build_id("sessions", "--home/wpp--"), "sessions-home-wpp");
    assert_eq!(crate::build_id("credentials", "default"), "credentials-default");
    assert_eq!(crate::build_id("sessions", "///"), "sessions-resource");
}

#[test]
fn a_single_session_can_be_extracted_and_injected_on_its_own() {
    let container = fixture_container("select");
    let payload = temp_dir("select-payload");
    let extracted = transfer::extract(
        &container,
        "profile/sessions",
        Some("--home-wpp--/session-1"),
        &payload,
        false,
    )
    .unwrap();
    assert_eq!(extracted.files, 1, "only the selected session travels");
    assert!(payload.join("--home-wpp--/session-1/session.v3.jsonl.zstd").is_file());

    let target = temp_dir("select-target");
    transfer::inject(
        &payload, &target, "profile/sessions", Conflict::Refuse, Shape::Entries, 2, false,
    )
    .unwrap();
    assert!(target.join("profile/sessions/--home-wpp--/session-1/session.v3.jsonl.zstd").is_file());
    assert!(!target.join("profile/sessions/--home-wpp--/session-2").exists());
}
