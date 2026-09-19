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

/// The shipped dshell boxfile: a pinned harness tag and a pinned npm spec, the
/// two tokens that decide whether the build is reproducible at all.
#[test]
fn parses_the_dshell_boxfile() {
    let (script, _) = example("boxfile-dshell.dsh");

    assert_eq!(script.name, "dshell");
    assert_eq!(script.profile, "web");
    // A version tag, not a branch: the plugin's peer dependencies name exact DSH
    // releases, so `latest` would resolve to whatever shipped most recently.
    assert_eq!(script.harness_ref.as_deref(), Some("dsh-v0.1.6-alpha.1"));
    assert_eq!(
        script.harness_url,
        "https://github.com/deepseek-ai/deepseek-harness"
    );
    // The build refuses to install a lifecycle script until it is named, and the
    // bundle reaches `node-pty`, which compiles a native binding on install.
    assert_eq!(
        script.labels.get("dshbox.allow-build").map(String::as_str),
        Some("@nexus-aethra/dshell-bundle@0.1.3,node-pty@1.2.0-beta.15")
    );
    assert_eq!(script.ops.len(), 1);
    match script.ops.first() {
        Some(ImageOp::Add { kind, source, .. }) => {
            assert_eq!(*kind, AddKind::Plugin);
            // `@scope/name@version` carries two `@`, and the alias form
            // (`alias@npm:real@version`) is why the split is worth asserting.
            assert_eq!(
                source,
                &ParsedSource::NpmPrefix {
                    spec: "@nexus-aethra/dshell-bundle@0.1.3".to_owned()
                }
            );
        }
        other => panic!("expected one ADD plugin op, got {other:?}"),
    }
}
