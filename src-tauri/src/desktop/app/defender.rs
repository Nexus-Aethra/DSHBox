//! Windows Defender exclusion registration.
//!
//! Preparing a container materializes hundreds of thousands of small files
//! under `<runtime>/instances/.../harness/node_modules`. Windows Defender's
//! real-time scanner opens each freshly written file exactly while pnpm
//! reads it back for bin linking, which surfaces as `EBUSY` or pnpm's
//! `[UNKNOWN] unknown error, open '<...>/package.json'` and fails container
//! prepare deterministically on fresh installs. Excluding the install
//! directory and the user-selected runtime data directory from real-time
//! scanning removes the race.
//!
//! Registration needs administrator rights, so the desktop shell asks once
//! via UAC (`Start-Process -Verb RunAs`) and records the outcome in
//! `BoxConfig::defender_exclusions_for`. A new install/runtime pair triggers
/// at most one new prompt; a declined prompt is remembered so launching the
/// app never nags repeatedly.

use super::*;

/// Marker recorded when the user dismissed the UAC prompt, so the same
/// install/runtime pair is not re-prompted on every launch.
const DECLINED_PREFIX: &str = "declined:";

pub(crate) fn exclusion_key(install_dir: &Path, runtime_dir: &Path) -> String {
    format!("{}|{}", install_dir.display(), runtime_dir.display())
}

/// Launch the elevation flow on a background thread when the current
/// exclusion key differs from the recorded one. Never blocks startup.
pub(crate) fn ensure_defender_exclusions(install_dir: PathBuf, runtime_dir: PathBuf) {
    if !cfg!(windows) || !install_dir.is_dir() || !runtime_dir.is_dir() {
        return;
    }
    let key = exclusion_key(&install_dir, &runtime_dir);
    let Ok(config) = read_config() else {
        return;
    };
    match config.defender_exclusions_for.as_deref() {
        Some(recorded) if recorded == key || recorded == format!("{DECLINED_PREFIX}{key}") => {
            return;
        }
        _ => {}
    }
    thread::spawn(move || register_defender_exclusions(install_dir, runtime_dir, key));
}

#[cfg(windows)]
fn register_defender_exclusions(install_dir: PathBuf, runtime_dir: PathBuf, key: String) {
    let stamp = now_seconds();
    let script = env::temp_dir().join(format!("dshbox-defender-{stamp}.ps1"));
    let result = env::temp_dir().join(format!("dshbox-defender-{stamp}.result"));
    let _ = fs::remove_file(&result);
    if fs::write(&script, exclusion_script()).is_err() {
        write_startup_log("defender exclusions: cannot write elevation helper script");
        return;
    }
    let inner = format!(
        "-NoProfile -ExecutionPolicy Bypass -File \"{}\" -InstallDir \"{}\" -RuntimeDir \"{}\" -ResultFile \"{}\"",
        script.display(),
        install_dir.display(),
        runtime_dir.display(),
        result.display()
    );
    let encoded = encode_utf16le_base64(&inner);
    // Start-Process -Verb RunAs shows the UAC prompt; -PassThru -Wait lets us
    // read the elevated process exit code. A dismissal throws, which the
    // outer powershell surfaces as a non-zero exit.
    let outer = format!(
        "$p = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru \
         -ArgumentList '-NoProfile','-EncodedCommand','{encoded}'; exit $p.ExitCode"
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &outer])
        .status();
    let _ = fs::remove_file(&script);
    let outcome = match status {
        Ok(exit) if exit.success() => fs::read_to_string(&result).unwrap_or_default(),
        _ => String::from("declined"),
    };
    let _ = fs::remove_file(&result);
    let trimmed = outcome.trim();
    let recorded = if trimmed == "ok" || trimmed == "unavailable" {
        write_startup_log(&format!("defender exclusions registered ({trimmed}): {}", key));
        key
    } else if trimmed == "declined" {
        write_startup_log("defender exclusions: UAC prompt declined; container prepare may hit transient file locks");
        format!("{DECLINED_PREFIX}{key}")
    } else {
        write_startup_log(&format!(
            "defender exclusions failed: {trimmed}; container prepare may hit transient file locks"
        ));
        format!("{DECLINED_PREFIX}{key}")
    };
    if let Ok(mut config) = read_config() {
        config.defender_exclusions_for = Some(recorded);
        let _ = write_config(&config);
    }
}

#[cfg(not(windows))]
fn register_defender_exclusions(_install_dir: PathBuf, _runtime_dir: PathBuf, _key: String) {}

/// Elevated helper: add both exclusions idempotently. Hosts without the
/// Defender interface (Server Core, third-party AV) report `unavailable`
/// instead of failing so we do not re-prompt them either.
#[cfg(windows)]
fn exclusion_script() -> String {
    r#"param(
  [string]$InstallDir,
  [string]$RuntimeDir,
  [string]$ResultFile
)
$ErrorActionPreference = 'Stop'
$status = 'ok'
foreach ($path in @($InstallDir, $RuntimeDir)) {
  if ([string]::IsNullOrWhiteSpace($path) -or -not (Test-Path -LiteralPath $path)) { continue }
  try {
    Add-MpPreference -ExclusionPath $path -ErrorAction Stop
  } catch {
    $message = "$($_.Exception.Message)"
    if ($message -match 'is not recognized|no such interface|0x80004002') {
      $status = 'unavailable'
    } else {
      $status = "error: $message"
    }
  }
}
Set-Content -LiteralPath $ResultFile -Value $status -Encoding ASCII
"#
    .to_owned()
}

/// PowerShell `-EncodedCommand` expects base64 of UTF-16LE. Hand-rolled to
/// avoid a new dependency for this one call site.
#[cfg(windows)]
fn encode_utf16le_base64(value: &str) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bytes = Vec::with_capacity(value.len() * 2);
    for unit in value.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        encoded.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        encoded.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_powershell_encoded_command_format() {
        // Reference encoding produced by [Convert]::ToBase64String of UTF-16LE.
        assert_eq!(encode_utf16le_base64("ok"), "bwBrAA==");
        assert_eq!(encode_utf16le_base64(""), "");
        let long = "-NoProfile -ExecutionPolicy Bypass -File C:\\temp\\dsh box.ps1";
        let encoded = encode_utf16le_base64(long);
        assert!(!encoded.contains('\n') && !encoded.contains(' '));
        assert_eq!(encoded.len() % 4, 0);
    }

    #[test]
    fn exclusion_key_pairs_install_and_runtime_dirs() {
        assert_eq!(
            exclusion_key(Path::new("C:\\App"), Path::new("D:\\Data")),
            "C:\\App|D:\\Data"
        );
    }
}
