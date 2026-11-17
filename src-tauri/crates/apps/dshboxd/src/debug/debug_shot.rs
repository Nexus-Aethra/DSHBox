//! Screenshots: capture the page as a PNG, size it, prune the old ones.
//!
//! Self-contained -- the only CDP call it makes is Page.captureScreenshot, and
//! the pruning and dimension logic is pure. Nothing here knows about the
//! session table or about clicks and queries.

use super::*;

/// Where one container's screenshots are written.
///
/// A sibling of the browser profile rather than a directory inside it: the
/// profile belongs to Chrome and a debugging artefact does not, and Chrome is
/// free to tidy its own user-data-dir.
pub(super) fn screenshot_dir(state: &DaemonState, id: &str) -> Result<PathBuf, String> {
    let root = state
        .paths
        .read()
        .map_err(|_| "daemon paths lock failed".to_owned())?;
    let runtime = root
        .runtime
        .as_ref()
        .ok_or_else(|| "no runtime directory configured".to_owned())?;
    Ok(runtime.join("debug-screenshots").join(id))
}

/// Screenshots kept per container before the oldest are dropped.
///
/// A debugging session can take one per step and nothing else in the box ever
/// deletes these, so an unbounded directory is the default otherwise. The cap
/// keeps the newest -- the ones a human is most likely to still be opening --
/// and lets the rest go.
const MAX_SCREENSHOTS: usize = 50;

pub(super) fn prune_screenshots(directory: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "png"))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    let excess = files.len().saturating_sub(keep);
    if excess == 0 {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified);
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Width and height out of a PNG's IHDR, without pulling in an image decoder.
///
/// The eight-byte signature, a four-byte chunk length, the four bytes "IHDR",
/// then width and height as big-endian u32 -- so the whole header is the first
/// 24 bytes and nothing after it has to be read.
pub(super) fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..24)?;
    if &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
    let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
    Some((width, height))
}

/// One screenshot that has been decoded, measured and written.
pub(super) struct WrittenShot {
    pub(super) path: PathBuf,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bytes: usize,
}

impl std::fmt::Debug for WrittenShot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WrittenShot")
            .field("path", &self.path)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// Turn what Chrome sent into a file, and say what landed where.
///
/// Everything a caller is told about the image comes out of the bytes rather
/// than out of the request, so a reply claiming 1920x1080 describes a PNG that
/// exists on disk with that size.
pub(super) fn write_screenshot(directory: &PathBuf, encoded: &str) -> Result<WrittenShot, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| format!("screenshot carried undecodable base64: {error}"))?;
    // A file that cannot be measured is a file nobody asked for, so this is an
    // error rather than a null: Chrome answered with something that is not the
    // PNG it was asked for.
    let (width, height) = png_dimensions(&bytes)
        .ok_or_else(|| "screenshot is not a PNG the daemon can measure".to_owned())?;
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    let path = directory.join(format!("{stamp}.png"));
    std::fs::write(&path, &bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    prune_screenshots(directory, MAX_SCREENSHOTS);
    Ok(WrittenShot {
        path,
        width,
        height,
        bytes: bytes.len(),
    })
}


pub(crate) fn screenshot_rpc(state: &DaemonState, request: &Value) -> Result<Value, String> {
    // PNG only, and that is a contract rather than a limitation: the answer
    // carries a path and the image's size, and reading a PNG header for its
    // dimensions is a few bytes of arithmetic where jpeg and webp each want a
    // parser of their own. A caller that needs another format converts the file
    // that was written.
    let format = request["format"].as_str().unwrap_or("png");
    if format != "png" {
        return Err(
            "screenshots are written as png; convert the file for another format".to_owned(),
        );
    }
    let full_page = request["fullPage"].as_bool().unwrap_or(false);
    let mut params = json!({ "format": "png", "captureBeyondViewport": full_page });
    if full_page {
        // captureBeyondViewport only produces a full-page image when the clip
        // is widened; without this the result is the viewport anyway.
        let metrics = with_session(state, request, |session| {
            session.call("Page.getLayoutMetrics", json!({}))
        })?;
        let content = &metrics["cssContentSize"];
        let width = content["width"].as_f64().unwrap_or(0.0);
        let height = content["height"].as_f64().unwrap_or(0.0);
        if width > 0.0 && height > 0.0 {
            params["clip"] = json!({
                "x": 0.0,
                "y": 0.0,
                "width": width,
                "height": height,
                "scale": 1
            });
        }
    }
    let id = session_id(state, request)?;
    let data = with_session(state, request, |session| {
        let result = session.call("Page.captureScreenshot", params)?;
        result
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "Page.captureScreenshot returned no image".to_owned())
    })?;
    let shot = write_screenshot(&screenshot_dir(state, &id)?, &data)?;
    // The PNG is already on disk; a page that closed mid-call must not turn
    // a successful capture into a failed tool call, so the location degrades
    // to what the session still remembers (no CDP round-trip, no failure).
    let (url, title) = with_session(state, request, page_location).or_else(|_| {
        with_session(state, request, |session| {
            Ok::<(String, Option<String>), String>((session.url().to_owned(), None))
        })
    })?;
    Ok(json!({
        "id": id,
        "path": shot.path,
        "width": shot.width,
        "height": shot.height,
        "bytes": shot.bytes,
        "url": url,
        "title": title,
    }))
}
