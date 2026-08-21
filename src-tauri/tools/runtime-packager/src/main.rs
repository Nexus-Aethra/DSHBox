use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
    git: BTreeMap<String, GitAsset>,
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
#[derive(Clone, Deserialize)]
struct GitAsset {
    version: String,
    url: String,
    sha256: String,
    entry: String,
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
    git_version: String,
    git_entry: String,
    git_sha256: String,
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
    let git = lock.git.get(&target).cloned();
    // Git is best-effort: the runtime works for everything except Git-backed
    // pnpm specs without it. A missing 7z (or a transient network error)
    // should not block the Node+pnpm extraction that other DSH Box features
    // already depend on. The manifest reflects actual state — empty git
    // fields mean "not bundled" — and the warning names the install step the
    // developer must run.
    let installed_git: Option<GitAsset> = match git {
        Some(asset) => match extract_git(&asset, &cache, &output) {
            Ok(()) => Some(asset),
            Err(reason) => {
                eprintln!(
                    "warning: bundled git extraction failed ({reason}); pnpm will not be able \
                     to resolve `github:` specs without a system Git or a re-run of \
                     `pnpm runtime:prepare` after fixing the cause"
                );
                None
            }
        },
        None => {
            println!("skipping bundled git: no entry for target {target}");
            None
        }
    };
    let manifest = Manifest {
        target: target.clone(),
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
        git_version: installed_git.as_ref().map(|asset| asset.version.clone()).unwrap_or_default(),
        git_entry: installed_git.as_ref().map(|asset| asset.entry.clone()).unwrap_or_default(),
        git_sha256: installed_git.as_ref().map(|asset| asset.sha256.clone()).unwrap_or_default(),
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

/// Drive the full bundled-Git extraction flow: locate 7z, fetch the
/// archive, verify SHA-256, extract, and copy license files. Returns the
/// failure reason as a string so the caller can surface it in the warning.
fn extract_git(asset: &GitAsset, cache: &Path, output: &Path) -> Result<(), String> {
    let seven_zip = find_7z().ok_or_else(seven_zip_install_hint)?;
    let bytes = cached_download(cache, &asset.url)?;
    verify_sha256(&bytes, &asset.sha256)?;
    let destination = output.join("git");
    unpack_7z(&bytes, &destination, &seven_zip)?;
    if !destination.join(&asset.entry).is_file() {
        return Err(format!(
            "bundled git archive did not contain expected entry {}",
            asset.entry
        ));
    }
    collect_git_licenses(&destination, &output.join("LICENSES"))?;
    Ok(())
}

/// Locate a usable 7-Zip executable. The packager needs `7z` to expand the
/// PortableGit self-extracting archive; DSH Box does not bundle 7-Zip itself
/// because it is only required during `runtime:prepare` and never reaches
/// end users (only the unpacked `git/` tree ships in the installer).
fn find_7z() -> Option<PathBuf> {
    if let Ok(path) = which_7z("7z") {
        return Some(path);
    }
    if let Ok(path) = which_7z("7z.exe") {
        return Some(path);
    }
    #[cfg(windows)]
    {
        for env_name in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(root) = env::var_os(env_name) {
                let candidate = PathBuf::from(root).join("7-Zip").join("7z.exe");
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn which_7z(name: &str) -> Result<PathBuf, String> {
    let output = Command::new(name).arg("--help").stdout(Stdio::null()).stderr(Stdio::null()).output();
    match output {
        Ok(_) => Ok(PathBuf::from(name)),
        Err(error) => Err(error.to_string()),
    }
}

/// Rendered into the runtime-packager error when `find_7z` returns `None`.
/// Pointing the developer at the official installer keeps the project's
/// own dependency surface narrow: 7-Zip is a one-shot unpack tool, not part
/// of the runtime DSH Box ships.
fn seven_zip_install_hint() -> String {
    if cfg!(windows) {
        "PortableGit is distributed as a 7z self-extracting archive, but no \
         `7z` executable was found on PATH or under %ProgramFiles%\\7-Zip. \
         Install 7-Zip from <https://www.7-zip.org/> and ensure `7z` is on \
         PATH, then re-run `pnpm runtime:prepare`."
            .to_owned()
    } else if cfg!(target_os = "macos") {
        "PortableGit is distributed as a 7z self-extracting archive, but no \
         `7z` executable was found on PATH. Install p7zip via Homebrew \
         (`brew install p7zip`) or your package manager of choice, then \
         re-run `pnpm runtime:prepare`."
            .to_owned()
    } else {
        "PortableGit is distributed as a 7z self-extracting archive, but no \
         `7z` executable was found on PATH. Install p7zip-full \
         (`apt-get install p7zip-full` on Debian/Ubuntu, equivalent on your \
         distribution) then re-run `pnpm runtime:prepare`."
            .to_owned()
    }
}

/// Drive `7z` to extract a self-extracting archive into `destination`. The
/// archive bytes are written to a sibling temp file because `7z` only takes
/// a path, not stdin. The temp file is removed regardless of outcome.
fn unpack_7z(bytes: &[u8], destination: &Path, seven_zip: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(stringify)?;
    let parent = destination
        .parent()
        .ok_or_else(|| format!("git destination {} has no parent", destination.display()))?;
    let temp_archive = parent.join(format!(
        ".git-archive-{}.7z.exe",
        std::process::id()
    ));
    fs::write(&temp_archive, bytes).map_err(stringify)?;
    let result = Command::new(seven_zip)
        .arg("x")
        .arg("-y")
        .arg(format!("-o{}", destination.display()))
        .arg(&temp_archive)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();
    let _ = fs::remove_file(&temp_archive);
    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "7z exited with {status} while extracting bundled git; ensure 7z is installed and PATH is correct"
        )),
        Err(error) => Err(format!("cannot launch 7z at {}: {error}", seven_zip.display())),
    }
}

/// Copy Git's license/notices into a top-level `LICENSES/` directory. Git
/// for Windows places `COPYING.txt`, `LICENSE.txt`, `NOTICE.txt`, and a
/// per-component `Licenses/` subtree at the archive root.
fn collect_git_licenses(git_root: &Path, licenses_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(licenses_dir).map_err(stringify)?;
    let mut queue: Vec<PathBuf> = vec![git_root.to_path_buf()];
    let mut index = 0;
    while index < queue.len() {
        let current = queue[index].clone();
        index += 1;
        let entries = fs::read_dir(&current).map_err(stringify)?;
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(value) => value,
                Err(_) => continue,
            };
            let path = entry.path();
            if file_type.is_dir() {
                let name_os = entry.file_name();
                let name = name_os.to_string_lossy();
                // Skip PortableGit's own bundled Licenses/ subtree here; we
                // want only the top-level notices to ship with the runtime.
                if current.as_path() == git_root && name.eq_ignore_ascii_case("Licenses") {
                    continue;
                }
                queue.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let owned_name = match entry.file_name().to_str() {
                Some(value) => value.to_owned(),
                None => continue,
            };
            let lower = owned_name.to_ascii_lowercase();
            if !(lower.starts_with("license")
                || lower.starts_with("notice")
                || lower == "copying.txt"
                || lower == "readme.md")
            {
                continue;
            }
            let target = licenses_dir.join(format!("git-{owned_name}"));
            fs::copy(&path, &target).map_err(stringify)?;
        }
    }
    Ok(())
}
