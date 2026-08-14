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
        npm_entry: "node/lib/node_modules/npm/bin/npm-cli.js".into(),
        pnpm_entry: "pnpm/node_modules/pnpm/bin/pnpm.cjs".into(),
    };
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
            if relative.as_os_str().is_empty() {
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
