use crate::kinds::{Conflict, ResolvedKind, Shape};
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
    assert_eq!(sessions.path(), "profile/sessions");
    assert_eq!(sessions.shape, Shape::Entries);
    assert!(!sessions.secret);
    assert_eq!(sessions.parts.len(), 1);

    let credentials = crate::builtin("credentials").expect("credentials kind");
    assert_eq!(credentials.path(), "profile/.credentials.yaml");
    assert!(credentials.secret, "credentials are a secret kind");
    // A key is only half of it: the provider route that names its environment
    // variable travels too, and where that lives is a version fact — hence the
    // two candidates for one slot.
    assert_eq!(credentials.parts.len(), 3);
    assert_eq!(credentials.parts[1].path, "profile/settings.yaml");
    assert_eq!(credentials.parts[1].section, ["llm-pi-ai"]);
    assert_eq!(credentials.parts[1].slot, Some("provider-route"));
    assert_eq!(credentials.parts[2].slot, Some("provider-route"));
    assert!(crate::builtin("sshkey").is_none());
}

#[test]
fn the_provider_route_slot_follows_the_containers_harness_version() {
    let credentials = crate::builtin("credentials").expect("credentials kind");
    let route = |version: Option<&str>| {
        ResolvedKind::from_builtin(&credentials, "web", version)
            .payload_parts()
            .remove(1)
    };

    let before = route(Some("dsh-v0.1.6-alpha.2"));
    assert_eq!(before.path, "profile/settings.yaml");
    assert_eq!(before.section, ["llm-pi-ai"]);

    // From 0.1.7 the profile keeps its overrides in a Cordis layer list, and the
    // path is per profile — which is why it carries `{profile}`.
    let after = route(Some("dsh-v0.1.7-alpha.1"));
    assert_eq!(after.path, "profile/profiles/web/cordis.patch.yml");
    assert_eq!(after.section, ["#llm-pi-ai"]);

    // A tree whose version cannot be read gets the evergreen declaration, not a
    // guess at the newest one.
    assert_eq!(route(None).path, "profile/settings.yaml");
    assert_eq!(crate::kinds::version_triple("dsh-v0.1.7-alpha.1"), Some((0, 1, 7)));
}

#[test]
fn a_layer_section_travels_the_cordis_item_it_names() {
    let root = temp_dir("cordis-layer");
    let rel = "profile/profiles/web/cordis.patch.yml";
    write(
        &root.join(rel),
        r#"- id: agent-loop
  name: "@deepseek-ai/dsh-agent-loop"
  config:
    maxParallelToolCalls: 20
- id: llm-pi-ai
  name: "@deepseek-ai/dsh-llm-pi-ai"
  config:
    providers:
      minimax-cn:
        apiKeyEnv: MINIMAX_CN_API_KEY
"#,
    );
    let route = vec!["#llm-pi-ai".to_owned()];

    // Merging a second provider in keeps the one already there and leaves the
    // other plugins' layers alone.
    transfer::write_section(
        &root,
        rel,
        &route,
        "providers:\n  step:\n    apiKeyEnv: STEP_API_KEY\n",
        Conflict::Merge,
    )
    .unwrap();
    let body = fs::read_to_string(root.join(rel)).unwrap();
    assert!(body.contains("minimax-cn"), "the route already there was replaced:\n{body}");
    assert!(body.contains("step"), "the incoming route was not merged in:\n{body}");
    assert!(
        body.contains("maxParallelToolCalls"),
        "another plugin's layer was dropped:\n{body}"
    );

    // A layer that does not exist yet is appended as one more item.
    transfer::write_section(
        &root,
        rel,
        &vec!["#agent-default-model".to_owned()],
        "provider: step\nmodel: x\n",
        Conflict::Merge,
    )
    .unwrap();
    let body = fs::read_to_string(root.join(rel)).unwrap();
    assert_eq!(body.matches("- id:").count(), 3, "expected three layers:\n{body}");

    // An existing layer is not overwritten silently.
    assert!(transfer::write_section(&root, rel, &route, "providers: {}\n", Conflict::Refuse).is_err());

    // Overwriting replaces the section as one value, and the item keeps the
    // `name` Cordis resolves it by.
    transfer::write_section(
        &root,
        rel,
        &route,
        "providers:\n  only:\n    apiKeyEnv: ONLY_KEY\n",
        Conflict::Overwrite,
    )
    .unwrap();
    let body = fs::read_to_string(root.join(rel)).unwrap();
    assert!(!body.contains("minimax-cn"), "overwrite left the old route:\n{body}");
    assert!(body.contains("only"), "overwrite lost the incoming route:\n{body}");
    assert!(
        body.contains("@deepseek-ai/dsh-llm-pi-ai"),
        "overwrite dropped the layer's package name:\n{body}"
    );
    assert!(body.contains("maxParallelToolCalls"), "overwrite dropped another layer:\n{body}");
    let _ = fs::remove_dir_all(&root);
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

/// The provider credentials kind is two files, because a key authenticates
/// nothing until a route names the environment variable that holds it.
fn credentials_parts() -> Vec<transfer::PayloadPart> {
    vec![
        transfer::PayloadPart {
            path: "profile/.credentials.yaml".to_owned(),
            section: Vec::new(),
            slot: None,
        },
        transfer::PayloadPart {
            path: "profile/settings.yaml".to_owned(),
            section: vec!["llm-pi-ai".to_owned()],
            slot: None,
        },
    ]
}

fn credentials_fixture(name: &str) -> PathBuf {
    let root = temp_dir(name);
    write(
        &root.join("profile/.credentials.yaml"),
        "version: 1\nrefs:\n  MINIMAX_CN_API_KEY: sk-one\n",
    );
    write(
        &root.join("profile/settings.yaml"),
        "ui-onboarding:\n  welcomeNoticeVersion: 2026-08-13.1\nllm-pi-ai:\n  providers:\n    minimax-cn:\n      apiKeyEnv: MINIMAX_CN_API_KEY\n",
    );
    root
}

#[test]
fn provider_credentials_travel_as_a_key_and_the_route_that_names_it() {
    let source = credentials_fixture("provider-source");
    let payload = temp_dir("provider-payload");
    let extracted = transfer::extract_parts(&source, &credentials_parts(), None, &payload, true).unwrap();
    assert_eq!(extracted.files, 2, "the key file and the section");
    // The section travels as its subtree alone: the rest of settings.yaml is
    // another plugin's business.
    let section = fs::read_to_string(payload.join("part-1/section.yaml")).unwrap();
    assert!(section.contains("apiKeyEnv"), "the route is in the payload: {section}");
    assert!(!section.contains("ui-onboarding"), "and nothing else is: {section}");
    // The payload describes itself, so an injection does not have to guess.
    let parts = transfer::payload_parts(&payload, "profile/.credentials.yaml");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1].section, vec!["llm-pi-ai".to_owned()]);

    // Into a container that has settings of its own and no provider yet.
    let target = temp_dir("provider-target");
    write(&target.join("profile/settings.yaml"), "ui-onboarding:\n  welcomeNoticeVersion: keep\n");
    let injected = transfer::inject_parts(&payload, &target, &parts, Conflict::Merge, Shape::Opaque, 1, true).unwrap();
    assert_eq!(injected.files, 2);
    assert!(injected.added.iter().any(|entry| entry == "llm-pi-ai"), "the route section is what was added: {:?}", injected.added);

    let settings: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(target.join("profile/settings.yaml")).unwrap()).unwrap();
    assert_eq!(settings["ui-onboarding"]["welcomeNoticeVersion"].as_str(), Some("keep"));
    assert_eq!(
        settings["llm-pi-ai"]["providers"]["minimax-cn"]["apiKeyEnv"].as_str(),
        Some("MINIMAX_CN_API_KEY")
    );
    let keys: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(target.join("profile/.credentials.yaml")).unwrap()).unwrap();
    assert_eq!(keys["refs"]["MINIMAX_CN_API_KEY"].as_str(), Some("sk-one"));
}

#[test]
fn a_section_refuses_overwrites_and_merges_by_policy() {
    let source = credentials_fixture("section-source");
    let payload = temp_dir("section-payload");
    transfer::extract_parts(&source, &credentials_parts(), None, &payload, false).unwrap();
    let parts = credentials_parts();

    // The target already has the section: refusing is the default.
    let target = credentials_fixture("section-target");
    write(
        &target.join("profile/settings.yaml"),
        "llm-pi-ai:\n  providers:\n    other:\n      apiKeyEnv: OTHER_KEY\n",
    );
    let refused = transfer::inject_parts(&payload, &target, &parts, Conflict::Refuse, Shape::Opaque, 1, false);
    assert!(refused.is_err(), "refusing is the default");
    let untouched = fs::read_to_string(target.join("profile/settings.yaml")).unwrap();
    assert!(untouched.contains("OTHER_KEY"), "a refused injection writes nothing");

    // Merging keeps the route that was there and adds the payload's.
    transfer::inject_parts(&payload, &target, &parts, Conflict::Merge, Shape::Opaque, 1, false).unwrap();
    let merged: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(target.join("profile/settings.yaml")).unwrap()).unwrap();
    assert_eq!(merged["llm-pi-ai"]["providers"]["other"]["apiKeyEnv"].as_str(), Some("OTHER_KEY"));
    assert_eq!(
        merged["llm-pi-ai"]["providers"]["minimax-cn"]["apiKeyEnv"].as_str(),
        Some("MINIMAX_CN_API_KEY")
    );

    // Overwriting replaces the section wholesale.
    transfer::inject_parts(&payload, &target, &parts, Conflict::Overwrite, Shape::Opaque, 1, false).unwrap();
    let replaced: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(target.join("profile/settings.yaml")).unwrap()).unwrap();
    assert!(replaced["llm-pi-ai"]["providers"]["other"].is_null());
}

#[test]
fn a_single_part_payload_still_injects_as_the_path_it_names() {
    // Copies taken before kinds had parts have no manifest: the path the
    // caller names is the whole story.
    let payload = temp_dir("legacy-payload");
    write(&payload.join(".credentials.yaml"), "version: 1\n");
    let parts = transfer::payload_parts(&payload, "profile/.credentials.yaml");
    assert_eq!(parts.len(), 1);
    assert!(parts[0].section.is_empty());

    let target = temp_dir("legacy-target");
    transfer::inject_parts(&payload, &target, &parts, Conflict::Merge, Shape::Opaque, 1, false).unwrap();
    assert!(target.join("profile/.credentials.yaml").is_file());
}
