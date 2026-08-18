use std::path::{Path, PathBuf};
use std::env;

fn main() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dist_source = manifest_dir.parent().unwrap().join("dist");
    let dist_build = manifest_dir.join("dist");

    // tauri_build::build() validates frontendDist relative to src-tauri/.
    // Copy the built frontend there so validation passes.
    if dist_source.exists() {
        let _ = std::fs::remove_dir_all(&dist_build);
        if dist_source.is_dir() {
            copy_dir_all(&dist_source, &dist_build).ok();
        }
    }

    tauri_build::build();

    // At runtime, resource_dir() returns target/release/ (the binary's parent).
    // frontendDist is joined with resource_dir(), so copy dist there too.
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // OUT_DIR is target/<profile>/build/dshbox-<hash>/out
    let target_profile = out_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let dist_runtime = target_profile.join("dist");
    if dist_source.exists() {
        let _ = std::fs::remove_dir_all(&dist_runtime);
        if dist_source.is_dir() {
            copy_dir_all(&dist_source, &dist_runtime).unwrap_or_else(|error| {
                println!("cargo:warning=cannot copy dist/ to {}: {error}", dist_runtime.display())
            });
        }
    }
    println!("cargo:rerun-if-changed={}", dist_source.display());

    let stamp_path = manifest_dir.join(".build-stamp");
    println!("cargo:rerun-if-changed={}", stamp_path.display());
    let stamp = std::fs::read_to_string(&stamp_path).unwrap_or_default();
    println!("cargo:rustc-env=DSHBOX_BUILD_STAMP={}", stamp.trim());
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}
