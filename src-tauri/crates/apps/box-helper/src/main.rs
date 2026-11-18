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

mod node_link;

struct Args {
    enroll_token: String,
    control_url: String,
    node_id: String,
    kind: String,
    platform: String,
    /// The material for the encrypted link, when the operator supplied it.
    /// Without it the helper runs exactly as it did in stage A.
    link: Option<node_link::LinkConfig>,
}

/// Read the arguments from the process.
fn parse_args() -> Result<Args, String> {
    parse_from(env::args().skip(1))
}

/// Parse the arguments from an iterator.
///
/// Split out from `parse_args` so it can be tested without a process to read
/// from: the parsing is where a bad flag becomes a silently wrong
/// configuration, and that is worth pinning.
fn parse_from<I: Iterator<Item = String>>(mut iter: I) -> Result<Args, String> {
    let mut args = Args {
        enroll_token: String::new(),
        control_url: String::new(),
        node_id: String::new(),
        kind: "wsl2".to_owned(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        link: None,
    };
    while let Some(flag) = iter.next() {
        let value = iter.next().ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--enroll" => args.enroll_token = value,
            "--control" => args.control_url = value,
            "--node-id" => args.node_id = value,
            "--kind" => args.kind = value,
            "--platform" => args.platform = value,
            // The bootstrap material for the encrypted link. Three values
            // that arrive together in the enrol reply, or not at all.
            "--link-seed" => {
                args.link.get_or_insert_with(Default::default).seed = value
            }
            "--link-fingerprint" => {
                args.link.get_or_insert_with(Default::default).pinned_fingerprint = value
            }
            "--link-pubkey" => {
                args.link.get_or_insert_with(Default::default).control_pubkey = value
            }
            "--link-chain-length" => {
                let length = value
                    .parse::<u32>()
                    .map_err(|e| format!("bad chain length: {e}"))?;
                // A chain of fewer than two links has no spendable link
                // that is not the seed, and a one-link chain would send the
                // seed itself -- the one value the node must never put on
                // the wire. Refusing it here is better than the silent floor
                // the client would otherwise apply.
                if length < 2 {
                    return Err(format!(
                        "chain length must be at least 2, got {length}"
                    ));
                }
                args.link.get_or_insert_with(Default::default).chain_length = length
            }
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

/// The seed, decoded once. It is the root of the chain walk, and a bad one
/// is a configuration error rather than something to discover at handshake
/// time.
fn seed_bytes(config: &node_link::LinkConfig) -> Result<[u8; 32], String> {
    let bytes = node_link::control_pubkey_hex_to_bytes(&config.seed)
        .ok_or("the bootstrap seed is not valid hex")?;
    bytes
        .try_into()
        .map_err(|_| "the bootstrap seed is not 32 bytes".to_owned())
}

/// Open the encrypted link from the bootstrap material, or say why not.
///
/// A separate function rather than a nested match in `main`: the failure
/// paths are three deep, and nesting them inside the process entry point is
/// how a helper that cannot start its link becomes a helper that cannot
/// start.
fn open_link(
    config: &node_link::LinkConfig,
    node_id: &str,
    control_url: &str,
) -> Option<node_link::Link> {
    let seed = match seed_bytes(config) {
        Ok(seed) => seed,
        Err(error) => {
            eprintln!("[helper] node link unavailable: {error}");
            return None;
        }
    };

    // The walk starts here and lives on the Link, so a reconnect spends the
    // next link rather than the first one again. It is NOT rebuilt per
    // process: a fresh walk starts at c_(n-1) again, and the control plane
    // already holds that value, so a restart would be refused. Persisting
    // the walk across processes is stage B.
    let walker = box_node_link::chain::ChainNode::new(seed, config.chain_length);
    match node_link::connect(config, node_id, control_url, walker) {
        Ok(link) => {
            println!("[helper] node link established");
            Some(link)
        }
        Err(error) => {
            // Not fatal. The HTTP path is the fallback and it works; saying
            // why the link did not open is what lets an operator fix it.
            eprintln!("[helper] node link unavailable: {error}");
            None
        }
    }
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

    // The encrypted link, if the operator supplied the material for it. It is
    // an upgrade rather than a replacement: the HTTP path above still works,
    // and a helper that cannot open the link keeps heartbeating over HTTP
    // exactly as it did before.
    let mut link = args
        .link
        .as_ref()
        .and_then(|config| open_link(config, &args.node_id, &args.control_url));

    let mut backoff = 1u64;
    loop {
        thread::sleep(Duration::from_secs(session.heartbeat_seconds));
        // Prefer the link: one encrypted frame instead of one HTTP POST, and
        // the heartbeat is the highest-frequency call the helper makes.
        let result = match link.as_mut() {
            Some(link) => link
                .call("node_heartbeat", serde_json::json!({ "epoch": session.epoch }))
                .map(|_| ()),
            None => heartbeat(&session, &host, port),
        };
        match result {
            Ok(()) => {
                backoff = 1;
                println!("[helper] heartbeat ok (epoch {})", session.epoch);
            }
            Err(error) => {
                eprintln!("[helper] heartbeat failed: {error}");
                // A link that stops working is reconnected rather than
                // abandoned: the transport is what broke, the node is still
                // alive, and the chain still has links. Falling back to HTTP
                // for the rest of the process life -- which is what this did
                // A link that stops working is reconnected rather than
                // abandoned: the transport is what broke, the node is still
                // alive, and the chain still has links. Falling back to HTTP
                // for the rest of the process life -- which is what this did
                // before -- meant a node that looked healthy while its
                // encrypted path was gone for good.
                let reconnected = match link.as_mut() {
                    Some(link) => match link.reconnect() {
                        Ok(()) => {
                            println!("[helper] node link reconnected");
                            true
                        }
                        Err(error) => {
                            // A chain that has run out needs a fresh enrol
                            // grant, and that is an operator action. Saying so
                            // is the difference between a limit and a silent
                            // degradation.
                            eprintln!("[helper] node link gone: {error}");
                            false
                        }
                    },
                    None => false,
                };
                if !reconnected {
                    link = None;
                    eprintln!("[helper] falling back to HTTP heartbeats");
                } else {
                    backoff = 1;
                }
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

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "node_link_tests.rs"]
mod node_link_tests;
