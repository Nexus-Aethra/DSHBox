use box_image::parse_script;
use std::path::PathBuf;

#[test]
fn parses_example_boxfile() {
    // Walk up from crates/box-image to the workspace root (DSHBox/).
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // src-tauri/
    path.pop(); // workspace root
    path.push("examples/boxfile.dsh");
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let base_dir = path.parent().unwrap().to_path_buf();
    let script = parse_script(&body, &base_dir).expect("example boxfile parses");

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
    assert_eq!(script.ops.len(), 3);
}
