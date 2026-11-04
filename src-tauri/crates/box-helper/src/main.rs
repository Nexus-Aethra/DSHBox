//! box-helper — roadmap stage A.
//!
//! A minimal, dependency-light agent that runs INSIDE a node (WSL2 guest,
//! microVM, container…) and does exactly four things:
//!   1. exchange a one-shot enroll grant for a limited session token,
//!   2. subscribe to the daemon's SSE stream,
//!   3. heartbeat every 30s,
//!   4. echo/report so an operator can see it is alive.
//!
//! It performs NO file operations in stage A. "Move all operations down to
//! the helper" starts in stage B. The HTTP client is hand-written over
//! std::net::TcpStream so the helper has no network dependency and builds
//! on any Linux target with just serde_json.

use std::{
    env,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process,
    thread,
    time::Duration,
};

const VERSION: &str = "0.1.0";

struct Args {
    enroll_token: String,
    control_url: String,
    node_id: String,
    kind: String,
    platform: String,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        enroll_token: String::new(),
        control_url: String::new(),
        node_id: String::new(),
        kind: "wsl2".to_owned(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    };
    let mut iter = env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter.next().ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--enroll" => args.enroll_token = value,
            "--control" => args.control_url = value,
            "--node-id" => args.node_id = value,
            "--kind" => args.kind = value,
            "--platform" => args.platform = value,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.enroll_token.is_empty() || args.control_url.is_empty() || args.node_id.is_empty() {
        return Err("usage: box-helper --enroll <token> --control <url> --node-id <id> [--kind k] [--platform p]".to_owned());
    }
    Ok(args)
}

/// Parse "http://host:port" into (host, port). Only plain HTTP on loopback /
/// the WSL gateway is used in stage A.
fn split_url(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported control URL: {url}"))?;
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| format!("control URL has no port: {url}"))?;
    Ok((
        host.to_owned(),
        port.parse::<u16>().map_err(|e| format!("bad port: {e}"))?,
    ))
}

/// One JSON-RPC POST. Returns the parsed `result` object or an error string.
fn rpc(host: &str, port: u16, body: &str) -> Result<serde_json::Value, String> {
    let mut stream = TcpStream::connect((host, port)).map_err(|e| format!("connect: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    write!(
        stream,
        "POST /rpc HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    )
    .map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| format!("flush: {e}"))?;
    let mut raw = String::new();
    stream
        .read_to_string(&mut raw)
        .map_err(|e| format!("read: {e}"))?;
    let payload = raw
        .split("\r\n\r\n")
        .nth(1)
        .ok_or_else(|| "malformed HTTP response".to_owned())?;
    let frame: serde_json::Value =
        serde_json::from_str(payload.trim()).map_err(|e| format!("bad JSON: {e}"))?;
    if !frame["ok"].as_bool().unwrap_or(false) {
        return Err(frame["error"].as_str().unwrap_or("rpc failed").to_owned());
    }
    Ok(frame["result"].clone())
}

struct Session {
    token: String,
    epoch: u64,
    heartbeat_seconds: u64,
}

fn register(args: &Args, host: &str, port: u16) -> Result<Session, String> {
    let body = serde_json::json!({
        "method": "node_register",
        "enrollToken": args.enroll_token,
        "nodeId": args.node_id,
        "kind": args.kind,
        "platform": args.platform,
        "capabilities": ["exec", "heartbeat"],
        "helperVersion": VERSION,
    })
    .to_string();
    let result = rpc(host, port, &body)?;
    Ok(Session {
        token: result["sessionToken"].as_str().unwrap_or("").to_owned(),
        epoch: result["epoch"].as_u64().unwrap_or(0),
        heartbeat_seconds: result["heartbeatSeconds"].as_u64().unwrap_or(30),
    })
}

fn heartbeat(session: &Session, host: &str, port: u16) -> Result<(), String> {
    let body = serde_json::json!({
        "method": "node_heartbeat",
        "token": session.token,
        "epoch": session.epoch,
    })
    .to_string();
    rpc(host, port, &body).map(|_| ())
}

fn echo(session: &Session, host: &str, port: u16, text: &str) -> Result<(), String> {
    let body = serde_json::json!({
        "method": "node_echo",
        "token": session.token,
        "epoch": session.epoch,
        "text": text,
    })
    .to_string();
    rpc(host, port, &body).map(|_| ())
}

fn report(session: &Session, host: &str, port: u16, line: &str) -> Result<(), String> {
    let body = serde_json::json!({
        "method": "node_report",
        "token": session.token,
        "epoch": session.epoch,
        "level": "info",
        "line": line,
    })
    .to_string();
    rpc(host, port, &body).map(|_| ())
}

/// Subscribe to /events in a background thread. Stage A only prints the
/// event names; stage C will use this channel to receive tasks.
fn start_sse(host: String, port: u16, token: String, epoch: u64) {
    thread::spawn(move || {
        let Ok(mut stream) = TcpStream::connect((host.as_str(), port)) else {
            return;
        };
        let request = format!(
            "GET /events?token={token}&epoch={epoch} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
        );
        if stream.write_all(request.as_bytes()).is_err() {
            return;
        }
        let _ = stream.flush();
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(name) = line.strip_prefix("event: ") {
                println!("[sse] {name}");
            }
        }
    });
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("box-helper: {error}");
            process::exit(2);
        }
    };
    let (host, port) = match split_url(&args.control_url) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("box-helper: {error}");
            process::exit(2);
        }
    };

    let session = match register(&args, &host, port) {
        Ok(s) => s,
        Err(error) => {
            eprintln!("box-helper: registration failed: {error}");
            process::exit(1);
        }
    };
    println!(
        "[helper] registered node={} epoch={} heartbeat={}s",
        args.node_id, session.epoch, session.heartbeat_seconds
    );
    start_sse(host.clone(), port, session.token.clone(), session.epoch);
    let _ = echo(&session, &host, port, "stage-A hello");
    let _ = report(&session, &host, port, &format!("helper {VERSION} online"));

    let mut backoff = 1u64;
    loop {
        thread::sleep(Duration::from_secs(session.heartbeat_seconds));
        match heartbeat(&session, &host, port) {
            Ok(()) => {
                backoff = 1;
                println!("[helper] heartbeat ok (epoch {})", session.epoch);
            }
            Err(error) => {
                eprintln!("[helper] heartbeat failed: {error}");
                // A stale epoch or a rotated token means the daemon restarted.
                // Stage A has no persisted enroll grant, so we cannot silently
                // re-enroll; report it and retry with a bounded backoff.
                if error.contains("epoch") || error.contains("401") || error.contains("session") {
                    eprintln!("[helper] control plane restarted; a fresh enroll token is required");
                }
                thread::sleep(Duration::from_secs(backoff.min(30)));
                backoff = (backoff * 2).min(30);
            }
        }
    }
}
