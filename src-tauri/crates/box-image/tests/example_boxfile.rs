use box_image::{parse_script, AddKind, ImageOp, ParsedSource};
use std::path::PathBuf;

fn example(name: &str) -> (box_image::ImageScript, PathBuf) {
    // Walk up from crates/box-image to the workspace root (DSHBox/).
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // src-tauri/
    path.pop(); // workspace root
    path.push(format!("examples/{name}"));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let base_dir = path.parent().unwrap().to_path_buf();
    (
        parse_script(&body, &base_dir).expect("example boxfile parses"),
        base_dir,
    )
}

#[test]
fn parses_example_boxfile() {
    let (script, _) = example("boxfile.dsh");

    assert_eq!(script.name, "team-stack");
    assert_eq!(script.version, "1.0.0");
    // `:latest` is the default-branch convention but the parser keeps it as
    // the ref tag so the templates list / version picker can show it.
    assert_eq!(script.harness_ref.as_deref(), Some("latest"));
    assert_eq!(script.profile, "web");
    assert_eq!(
        script.labels.get("maintainer").map(String::as_str),
        Some("alice@example.com")
    );
    assert_eq!(script.ops.len(), 4);
}

/// The shipped dshell boxfile: a pinned harness tag, a pinned bundle spec, and
/// the provider rows that must ride that same DSH build.
#[test]
fn parses_the_dshell_boxfile() {
    let (script, _) = example("boxfile-dshell.dsh");

    assert_eq!(script.name, "dshell");
    assert_eq!(script.profile, "web");
    // A version tag, not a branch: the bundle's peer dependencies name exact DSH
    // releases, so `latest` would resolve to whatever shipped most recently.
    assert_eq!(script.harness_ref.as_deref(), Some("dsh-v0.1.6-alpha.2"));
    assert_eq!(
        script.harness_url,
        "https://github.com/deepseek-ai/deepseek-harness"
    );
    // The build refuses to install a lifecycle script until it is named, and the
    // bundle reaches `node-pty`, which compiles a native binding on install.
    assert_eq!(
        script.labels.get("dshbox.allow-build").map(String::as_str),
        Some("@nexus-aethra/dshell-bundle@0.1.5,node-pty@1.2.0-beta.15")
    );

    let specs: Vec<String> = script
        .ops
        .iter()
        .map(|op| match op {
            ImageOp::Add { kind, source, .. } => {
                assert_eq!(*kind, AddKind::Plugin);
                // `@scope/name@version` carries two `@`, and the alias form
                // (`alias@npm:real@version`) is why the split is worth asserting.
                match source {
                    ParsedSource::NpmPrefix { spec } => spec.clone(),
                    other => panic!("expected an npm: spec, got {other:?}"),
                }
            }
            other => panic!("expected only ADD plugin ops, got {other:?}"),
        })
        .collect();
    assert_eq!(specs.len(), 8);
    assert_eq!(specs[0], "@nexus-aethra/dshell-bundle@0.1.5");
    // Every other row is a dsh provider pinned to the FROM line's exact version:
    // an unpinned one would drag a second DSH build into the same tree.
    let harness_version = script
        .harness_ref
        .as_deref()
        .expect("harness ref")
        .trim_start_matches("dsh-v");
    for spec in &specs[1..] {
        assert!(
            spec.starts_with("@deepseek-ai/dsh-") && spec.ends_with(&format!("@{harness_version}")),
            "{spec} is not a dsh provider pinned to {harness_version}"
        );
    }
}
