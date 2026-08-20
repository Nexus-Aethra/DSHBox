use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};
use tar::Archive;
use xz2::read::XzDecoder;

#[derive(Deserialize)]
struct Lock {
    #[serde(rename = "nodeVersion")]
    node_version: String,
    #[serde(rename = "pnpmVersion")]
    pnpm_version: String,
    pnpm: Pnpm,
    node: BTreeMap<String, Asset>,
}
#[derive(Deserialize)]
struct Pnpm {
    url: String,
    integrity: String,
}
#[derive(Deserialize)]
struct Asset {
    url: String,
    sha256: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    target: String,
    node_version: String,
    pnpm_version: String,
    node_sha256: String,
    pnpm_integrity: String,
    node_entry: String,
    npm_entry: String,
    pnpm_entry: String,
}

fn main() -> Result<(), String> {
    let target = args()?.ok_or("usage: runtime-packager --target <platform-arch>")?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .ok_or("cannot locate workspace root")?
        .to_path_buf();
    let lock: Lock = serde_json::from_str(
        &fs::read_to_string(root.join("runtime-lock.json")).map_err(stringify)?,
    )
    .map_err(stringify)?;
    let node = lock
        .node
        .get(&target)
        .ok_or_else(|| format!("unsupported runtime target: {target}"))?;
    let cache = dirs::cache_dir()
        .ok_or("cannot locate cache directory")?
        .join("dsh-box/runtime");
    let output = root.join("src-tauri/resources/runtime").join(&target);
    if output.exists() {
        fs::remove_dir_all(&output).map_err(stringify)?;
    }
    fs::create_dir_all(&output).map_err(stringify)?;
    let node_bytes = cached_download(&cache, &node.url)?;
    verify_sha256(&node_bytes, &node.sha256)?;
    unpack_node(
        &node_bytes,
        &output.join("node"),
        node.url.ends_with(".zip"),
    )?;
    let pnpm_bytes = cached_download(&cache, &lock.pnpm.url)?;
    verify_integrity(&pnpm_bytes, &lock.pnpm.integrity)?;
    unpack_tgz(
        &pnpm_bytes,
        &output.join("pnpm/node_modules/pnpm"),
        "package",
    )?;
    let windows = target.starts_with("win-");
    // pnpm's own `runDepsStatusCheck` re-spawns itself; with the ESM entry it
    // reuses the current node executable instead of resolving a bare `pnpm`
    // command from PATH (which would fail without a system install).
    let pnpm_entry = if output.join("pnpm/node_modules/pnpm/bin/pnpm.mjs").is_file() {
        "pnpm/node_modules/pnpm/bin/pnpm.mjs"
    } else {
        "pnpm/node_modules/pnpm/bin/pnpm.cjs"
    };
    let manifest = Manifest {
        target,
        node_version: lock.node_version,
        pnpm_version: lock.pnpm_version,
        node_sha256: node.sha256.clone(),
        pnpm_integrity: lock.pnpm.integrity,
        node_entry: if windows {
            "node/node.exe".into()
        } else {
            "node/bin/node".into()
        },
        // Windows Node archives place npm at node/node_modules/npm, while
        // Unix archives place it at node/lib/node_modules/npm.
        npm_entry: if windows {
            "node/node_modules/npm/bin/npm-cli.js".into()
        } else {
            "node/lib/node_modules/npm/bin/npm-cli.js".into()
        },
        pnpm_entry: pnpm_entry.into(),
    };
    install_command_shims(&output, windows)?;
    fs::write(
        output.join("runtime-manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(stringify)?,
    )
    .map_err(stringify)?;
    println!("prepared bundled runtime at {}", output.display());
    Ok(())
}

fn args() -> Result<Option<String>, String> {
    let values: Vec<_> = env::args().collect();
    Ok(values
        .iter()
        .position(|value| value == "--target")
        .map(|index| {
            values
                .get(index + 1)
                .cloned()
                .ok_or("--target requires a value")
        })
        .transpose()?)
}

/// Writes `pnpm`/`npm` command shims next to the bundled runtime so scripts
/// and pnpm's own dependency-status check can resolve the commands from PATH
/// without a system Node/npm/pnpm install. The desktop app prepends these
/// directories to PATH when spawning tools.
fn install_command_shims(output: &Path, windows: bool) -> Result<(), String> {
    let node_entry = if windows { "node.exe" } else { "bin/node" };
    if windows {
        fs::write(
            output.join("pnpm/pnpm.cmd"),
            "@echo off\r\n\"%~dp0..\\node\\node.exe\" \"%~dp0node_modules\\pnpm\\bin\\pnpm.cjs\" %*\r\n",
        )
        .map_err(stringify)?;
        fs::write(
            output.join("node/npm.cmd"),
            "@echo off\r\n\"%~dp0node.exe\" \"%~dp0node_modules\\npm\\bin\\npm-cli.js\" %*\r\n",
        )
        .map_err(stringify)?;
    } else {
        fs::write(
            output.join("pnpm/pnpm"),
            format!(
                "#!/bin/sh\nexec \"$(dirname \"$0\")/../{node_entry}\" \"$(dirname \"$0\")/node_modules/pnpm/bin/pnpm.cjs\" \"$@\"\n"
            ),
        )
        .map_err(stringify)?;
        // The Node upstream tarball ships `node/bin/{npm,npx,corepack}` as
        // symlinks into `node/lib/node_modules/...`. Symlinks survive normal
        // tarball extraction, yet the Tauri deb/rpm bundler dereferences them
        // into the symlink target's content — and the upstream scripts
        // hardcode a `require('../lib/cli.js')` that only resolves from the
        // symlinked path. Replace each with a $(dirname)-relative shim so
        // the installed binary is self-contained and works regardless of
        // how the installer handles symlinks (the script stays valid when
        // copied to a different absolute path during install).
        let bin_dir = output.join("node").join("bin");
        fs::create_dir_all(&bin_dir).map_err(stringify)?;
        let shims: &[(&str, &str)] = &[
            ("npm", "../lib/node_modules/npm/bin/npm-cli.js"),
            ("npx", "../lib/node_modules/npm/bin/npx-cli.js"),
            ("corepack", "../lib/node_modules/corepack/dist/corepack.js"),
        ];
        for (name, script) in shims {
            let path = bin_dir.join(name);
            // The Node upstream tarball ships these names as symlinks into
            // lib/node_modules/...; without removing the symlink first,
            // fs::write would clobber the target file. Use symlink_metadata
            // so we don't follow the link ourselves when deciding what to
            // delete.
            if let Ok(metadata) = fs::symlink_metadata(&path) {
                if metadata.file_type().is_symlink() || metadata.is_file() {
                    fs::remove_file(&path).map_err(stringify)?;
                }
            }
            // `$(dirname "$0")` is `node/bin/`; the shim lives there, so the
            // node binary is just `$(dirname "$0")/node` (no `bin/` prefix)
            // and the script path steps up one level to `node/lib/...`.
            let body = format!(
                "#!/bin/sh\nexec \"$(dirname \"$0\")/node\" \"$(dirname \"$0\")/{script}\" \"$@\"\n"
            );
            fs::write(&path, body).map_err(stringify)?;
            // Preserve executable bit: the symlink didn't carry one, and
            // fs::write resets perms to 0644 on Unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&path).map_err(stringify)?.permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&path, perms).map_err(stringify)?;
            }
        }
    }
    Ok(())
}

fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn cache_path(cache: &Path, url: &str) -> PathBuf {
    cache.join(url.rsplit('/').next().unwrap_or("runtime.bin"))
}
fn cached_download(cache: &Path, url: &str) -> Result<Vec<u8>, String> {
    let path = cache_path(cache, url);
    if path.is_file() {
        return fs::read(path).map_err(stringify);
    }
    fs::create_dir_all(cache).map_err(stringify)?;
    let bytes = reqwest::blocking::Client::builder()
        .user_agent("DSH-Box-runtime-packager/0.1")
        .build()
        .map_err(stringify)?
        .get(url)
        .send()
        .map_err(|error| format!("cannot download {url}: {error}"))?
        .error_for_status()
        .map_err(stringify)?
        .bytes()
        .map_err(stringify)?
        .to_vec();
    fs::write(&path, &bytes).map_err(stringify)?;
    Ok(bytes)
}
fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    (actual == expected)
        .then_some(())
        .ok_or_else(|| format!("SHA-256 mismatch: expected {expected}, got {actual}"))
}
fn verify_integrity(bytes: &[u8], expected: &str) -> Result<(), String> {
    let encoded = expected
        .strip_prefix("sha512-")
        .ok_or("unsupported pnpm integrity algorithm")?;
    let expected = STANDARD.decode(encoded).map_err(stringify)?;
    let actual = Sha512::digest(bytes);
    (actual.as_slice() == expected.as_slice())
        .then_some(())
        .ok_or("pnpm integrity mismatch".to_owned())
}
fn unpack_node(bytes: &[u8], destination: &Path, zip: bool) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(stringify)?;
    if zip {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(stringify)?;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(stringify)?;
            let relative = strip_first(Path::new(entry.name()))?;
            if relative.as_os_str().is_empty() || is_redundant_node_file(&relative) {
                continue;
            }
            let target = destination.join(relative);
            if entry.is_dir() {
                fs::create_dir_all(target).map_err(stringify)?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(stringify)?;
                }
                let mut output = fs::File::create(target).map_err(stringify)?;
                std::io::copy(&mut entry, &mut output).map_err(stringify)?;
            }
        }
    } else {
        let decoder = XzDecoder::new(Cursor::new(bytes));
        unpack_tar(Archive::new(decoder), destination, None)?;
    }
    Ok(())
}

/// Files that are never invoked by DSH Box and only bloat the installer.
/// The runtime is driven exclusively through the node executable plus the
/// npm-cli.js and pnpm.cjs entry points recorded in the manifest.
fn is_redundant_node_file(relative: &Path) -> bool {
    let components: Vec<_> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let first = components.first().map(String::as_str).unwrap_or("");
    // Corepack and its shims are unused; DSH Box pins pnpm itself.
    if first == "node_modules" && components.get(1).map(String::as_str) == Some("corepack") {
        return true;
    }
    // Unix archives ship C headers and man pages that the app never uses.
    if matches!(first, "include" | "share") {
        return true;
    }
    if components.len() == 1 {
        return matches!(
            first,
            "CHANGELOG.md"
                | "README.md"
                | "corepack"
                | "corepack.cmd"
                | "npm"
                | "npm.cmd"
                | "npm.ps1"
                | "npx"
                | "npx.cmd"
                | "npx.ps1"
                | "install_tools.bat"
                | "nodevars.bat"
        );
    }
    if first == "lib" && components.get(1).map(String::as_str) == Some("node_modules") {
        return is_redundant_npm_file(&components[2..]);
    }
    // Windows Node archives keep npm directly under node_modules.
    if first == "node_modules" && components.get(1).map(String::as_str) == Some("npm") {
        return is_redundant_npm_file(&components[2..]);
    }
    false
}

fn is_redundant_npm_file(relative: &[String]) -> bool {
    match relative.first().map(String::as_str) {
        Some("docs") | Some("man") => true,
        // Documentation is never needed at runtime; keep LICENSE files only.
        Some(_) => relative
            .last()
            .map(String::as_str)
            .is_some_and(|name| name.ends_with(".md") && !name.starts_with("LICENSE")),
        None => false,
    }
}

/// pnpm tarball entries that duplicate dist content or are never used.
fn is_redundant_pnpm_file(relative: &Path) -> bool {
    let components: Vec<_> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    if components.first().map(String::as_str) == Some("artifacts") {
        return true;
    }
    components
        .last()
        .map(String::as_str)
        .is_some_and(|name| name.ends_with(".md") && !name.starts_with("LICENSE"))
}
fn unpack_tgz(bytes: &[u8], destination: &Path, prefix: &str) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(stringify)?;
    unpack_tar(
        Archive::new(GzDecoder::new(Cursor::new(bytes))),
        destination,
        Some(prefix),
    )
}
fn unpack_tar<R: Read>(
    mut archive: Archive<R>,
    destination: &Path,
    required_prefix: Option<&str>,
) -> Result<(), String> {
    for entry in archive.entries().map_err(stringify)? {
        let mut entry = entry.map_err(stringify)?;
        let path = entry.path().map_err(stringify)?;
        let relative = match required_prefix {
            Some(prefix) => path
                .strip_prefix(prefix)
                .map_err(|_| "archive entry escapes package prefix".to_owned())?
                .to_path_buf(),
            None => strip_first(&path)?,
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let redundant = match required_prefix {
            Some(_) => is_redundant_pnpm_file(&relative),
            None => is_redundant_node_file(&relative),
        };
        if redundant {
            continue;
        }
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(stringify)?;
        }
        entry.unpack(target).map_err(stringify)?;
    }
    Ok(())
}
fn strip_first(path: &Path) -> Result<PathBuf, String> {
    Ok(path.components().skip(1).collect())
}
