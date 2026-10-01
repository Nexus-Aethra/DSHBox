//! Launching a headless browser and speaking the DevTools Protocol to it.
//!
//! The browser runs with its own throwaway --user-data-dir under the Box
//! runtime root, never the user's real profile: a debug session must not
//! inherit their cookies, extensions or logged-in state, and two concurrent
//! sessions must not fight over one profile lock.
//!
//! The port is left to the browser (--remote-debugging-port=0) and read back
//! from the DevToolsActivePort file it writes. Guessing a port and hoping is
//! how a second Box, or an unrelated service, ends up being driven by mistake.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

/// How long to wait for the browser to publish its debugging port.
const PORT_WAIT: Duration = Duration::from_secs(20);
/// How long a single CDP command may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on the DevTools HTTP replies we buffer, so a misbehaving endpoint
/// cannot make the daemon allocate without bound.
const HTTP_CAP: usize = 4 * 1024 * 1024;

/// A running headless browser plus the DevTools connection to one page.
pub struct BrowserSession {
    child: Option<Child>,
    user_data_dir: PathBuf,
    port: u16,
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    target_id: String,
    url: String,
}

impl BrowserSession {
    /// Start a headless browser on 'url' and attach to its first page target.
    pub fn launch(browser: &Path, url: &str, user_data_dir: PathBuf) -> Result<Self, String> {
        let user_data_dir = absolute(&user_data_dir);
        std::fs::create_dir_all(&user_data_dir)
            .map_err(|error| format!("cannot create browser profile dir: {error}"))?;

        // A DevToolsActivePort left behind by an earlier run names a port
        // nothing is listening on any more. The browser only overwrites it
        // once it is far enough along to bind, so without this delete the
        // wait below can read the stale port, return instantly, and then fail
        // to connect for the whole timeout while the real browser comes up
        // fine on a different port.
        let stale_port = user_data_dir.join("DevToolsActivePort");
        if stale_port.exists() {
            let _ = std::fs::remove_file(&stale_port);
        }

        let child = Command::new(browser)
            .arg("--headless=new")
            // 0 lets the OS pick a free port; we read it back from the file.
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            // A throwaway profile still inherits extensions from a machine-wide
            // policy, and an extension's injected UI lands inside the page. That
            // is fatal for this browser specifically: a popup shifts the page's
            // hit targets, so a screenshot shows a stranger's widget and a
            // coordinate click lands on it instead of the element underneath.
            .arg("--disable-extensions")
            .arg("--disable-gpu")
            .arg("--hide-scrollbars")
            .arg("--mute-audio")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // A debug browser's stderr is pure noise (telemetry, extension
            // chatter); the failure modes that matter are reported by
            // wait_for_port instead.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("cannot start browser {}: {error}", browser.display()))?;

        let mut child = child;
        let port = match wait_for_port(&user_data_dir, &mut child) {
            Ok(port) => port,
            Err(error) => {
                let _ = child.kill();
                let _ = std::fs::remove_dir_all(&user_data_dir);
                return Err(error);
            }
        };

        let target = match find_page_target(port, url) {
            Ok(target) => target,
            Err(error) => {
                let _ = child.kill();
                let _ = std::fs::remove_dir_all(&user_data_dir);
                return Err(error);
            }
        };

        // connect() hands back the handshake response alongside the socket;
        // the socket is the only part the session needs.
        let socket = connect(&target.web_socket_url)
            .map(|(socket, _response)| socket)
            .map_err(|error| format!("cannot open DevTools socket: {error}"))?;

        let mut session = Self {
            child: Some(child),
            user_data_dir,
            port,
            socket,
            next_id: 0,
            target_id: target.id,
            url: url.to_owned(),
        };
        // The protocol domains we depend on. Without these, captureScreenshot
        // and Runtime.evaluate fail with 'not enabled'.
        session.call("Page.enable", json!({}))?;
        session.call("Runtime.enable", json!({}))?;
        Ok(session)
    }

    /// DevTools port this session is attached to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// CDP target id of the attached page.
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    /// URL the browser was pointed at.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Send one CDP command and return its result field.
    ///
    /// CDP multiplexes replies over one socket, so replies are matched by
    /// id and anything that is not a reply for the outstanding command is
    /// skipped. Browser-emitted events (Page.loadEventFired and friends) show
    /// up on the same stream and must not be mistaken for a response.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({ "id": id, "method": method, "params": params });
        let deadline = Instant::now() + CALL_TIMEOUT;

        self.socket
            .send(Message::Text(request.to_string()))
            .map_err(|error| format!("cannot send {method}: {error}"))?;

        loop {
            if Instant::now() > deadline {
                return Err(format!("{method} timed out after {:?}", CALL_TIMEOUT));
            }
            let message = self
                .socket
                .read()
                .map_err(|error| format!("cannot read reply for {method}: {error}"))?;
            let text = match message {
                Message::Text(text) => text,
                Message::Close(_) => return Err(format!("browser closed the socket during {method}")),
                // Binary frames and pings carry no command reply.
                _ => continue,
            };
            let value: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                let detail = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown DevTools error");
                return Err(format!("{method} failed: {detail}"));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

impl Drop for BrowserSession {
    /// Tear the browser down with the session.
    ///
    /// The profile directory goes too: it is scratch space, and leaving one
    /// behind per debugging run would grow without bound.
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            #[cfg(windows)]
            {
                // Chrome spawns renderer and GPU children; killing only the
                // browser process leaves them holding the profile lock.
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(not(windows))]
            {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.user_data_dir);
    }
}

struct PageTarget {
    id: String,
    web_socket_url: String,
}

/// Read DevToolsActivePort until the browser publishes it.
fn wait_for_port(user_data_dir: &Path, child: &mut Child) -> Result<u16, String> {
    let path = user_data_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + PORT_WAIT;
    while Instant::now() < deadline {
        // A browser that died during startup must surface as an error now,
        // not as a 20-second timeout.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("browser exited during startup ({status})"));
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(port) = text.lines().next().and_then(|line| line.trim().parse().ok()) {
                return Ok(port);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("browser did not publish a DevTools port in time".to_owned())
}

/// Find the page target showing 'url'.
fn find_page_target(port: u16, url: &str) -> Result<PageTarget, String> {
    // The browser may still be opening the page; the list can briefly be
    // empty, so retry rather than fail the launch.
    let deadline = Instant::now() + PORT_WAIT;
    let mut last = String::new();
    while Instant::now() < deadline {
        match http_get_json(port, "/json/list") {
            Ok(Value::Array(targets)) => {
                let wanted = strip_query(url);
                let hit = targets.iter().find_map(|target| {
                    let target_url = target.get("url").and_then(Value::as_str)?;
                    let is_page = target.get("type").and_then(Value::as_str) == Some("page");
                    if !is_page || strip_query(target_url) != wanted {
                        return None;
                    }
                    Some(PageTarget {
                        id: target.get("id")?.as_str()?.to_owned(),
                        web_socket_url: target.get("webSocketDebuggerUrl")?.as_str()?.to_owned(),
                    })
                });
                if let Some(hit) = hit {
                    return Ok(hit);
                }
                // A page that redirects, or a non-http scheme such as data:,
                // is reported under a URL that never equals the one we asked
                // for. Fall back to the first page target rather than
                // refusing to attach: the browser was launched for exactly
                // one page, so a stray target is not a real ambiguity here.
                if let Some(fallback) = targets.iter().find_map(|target| {
                    if target.get("type").and_then(Value::as_str) != Some("page") {
                        return None;
                    }
                    Some(PageTarget {
                        id: target.get("id")?.as_str()?.to_owned(),
                        web_socket_url: target.get("webSocketDebuggerUrl")?.as_str()?.to_owned(),
                    })
                }) {
                    return Ok(fallback);
                }
                last = "no page target matched the requested URL".to_owned();
            }
            Ok(other) => last = format!("unexpected /json/list payload: {other}"),
            Err(error) => last = error,
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(format!("cannot attach to a page target for {url}: {last}"))
}

/// Compare page identity without the query string.
///
/// DSH Box appends a per-launch capability token, so the browser's reported
/// URL never matches the one we asked for byte for byte.
fn strip_query(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Minimal loopback HTTP GET returning a parsed JSON body.
///
/// Hand-rolled rather than pulled in as a dependency: the only endpoint used
/// is a loopback DevTools discovery reply, and this crate otherwise needs no
/// HTTP client. Handles both Content-Length and chunked bodies because
/// Chrome uses both across versions.
fn http_get_json(port: u16, path: &str) -> Result<Value, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|error| format!("cannot reach DevTools on 127.0.0.1:{port}: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: application/json\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| error.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .map_err(|error| format!("cannot read DevTools status line: {error}"))?;
    if !status.contains(" 200") {
        return Err(format!("DevTools answered {status:?}"));
    }

    let mut chunked = false;
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("cannot read DevTools headers: {error}"))?;
        if read == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().ok();
        } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }

    let body = if chunked {
        read_chunked(&mut reader)?
    } else {
        match length {
            Some(length) => {
                let mut buffer = vec![0u8; length];
                reader
                    .read_exact(&mut buffer)
                    .map_err(|error| format!("cannot read DevTools body: {error}"))?;
                buffer
            }
            // No length and not chunked: the socket close delimits the body.
            None => {
                let mut buffer = Vec::new();
                reader
                    .take(HTTP_CAP as u64)
                    .read_to_end(&mut buffer)
                    .map_err(|error| format!("cannot read DevTools body: {error}"))?;
                buffer
            }
        }
    };
    if body.len() > HTTP_CAP {
        return Err("DevTools reply exceeded the size cap".to_owned());
    }
    serde_json::from_slice(&body).map_err(|error| format!("DevTools reply is not JSON: {error}"))
}

fn read_chunked(reader: &mut impl BufRead) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("cannot read chunk size: {error}"))?;
        let size = usize::from_str_radix(
            line.trim().split(';').next().unwrap_or("0").trim(),
            16,
        )
        .map_err(|error| format!("bad chunk size {:?}: {error}", line.trim()))?;
        if size == 0 {
            break;
        }
        if body.len() + size > HTTP_CAP {
            return Err("DevTools reply exceeded the size cap".to_owned());
        }
        let mut chunk = vec![0u8; size];
        reader
            .read_exact(&mut chunk)
            .map_err(|error| format!("cannot read chunk body: {error}"))?;
        body.extend_from_slice(&chunk);
        // Consume the CRLF that terminates the chunk.
        let mut trailer = String::new();
        let _ = reader.read_line(&mut trailer);
    }
    Ok(body)
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}
