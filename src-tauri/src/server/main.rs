use box_server_core::{default_endpoint, remove_discovery, write_discovery};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
};

fn main() {
    #[cfg(not(unix))]
    {
        eprintln!("dshboxd named-pipe transport is not implemented for this target");
        std::process::exit(1);
    }
    #[cfg(unix)]
    run();
}

#[cfg(unix)]
fn run() {
    let endpoint = default_endpoint().expect("server endpoint");
    if endpoint.exists() {
        let _ = fs::remove_file(&endpoint);
    }
    let listener = UnixListener::bind(&endpoint).expect("bind DSH Box server socket");
    let discovery = write_discovery(endpoint.to_string_lossy()).expect("write server discovery");
    for stream in listener.incoming().flatten() {
        let token = discovery.token.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream.try_clone().expect("clone socket"));
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let valid = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|value| value["token"].as_str().map(|value| value == token))
                    .unwrap_or(false);
                let response = if valid {
                    serde_json::json!({"ok": true, "result": {"pid": std::process::id(), "status": "running"}})
                } else {
                    serde_json::json!({"ok": false, "error": "unauthorized"})
                };
                let mut stream = stream;
                let _ = writeln!(stream, "{response}");
            }
        });
    }
    remove_discovery();
}
