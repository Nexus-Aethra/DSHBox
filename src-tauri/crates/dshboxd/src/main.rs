//! DSH Box daemon — owns the task queue, the bundled runtime, the running
//! container registry, and every business operation. CLI and GUI are thin
//! clients talking JSON-over-unix-socket to this process (docker-style:
//! `dockerd` ↔ `docker`).

mod bundles;
mod containers;
mod data;
mod dispatch;
mod extensions;
mod image;
mod lifecycle;
mod state;
mod toolchains;
mod versions;

use box_server_core::{default_endpoint, remove_discovery, write_discovery};
use state::{initialize_bundled_runtime, DaemonState};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    sync::Arc,
};

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    pub fn run() {
        let endpoint = default_endpoint();
        if let Some(parent) = endpoint.parent() {
            fs::create_dir_all(parent).expect("create DSH Box server directory");
        }
        if endpoint.exists() {
            let _ = fs::remove_file(&endpoint);
        }
        let listener = UnixListener::bind(&endpoint).expect("bind DSH Box daemon socket");
        let discovery =
            write_discovery(endpoint.to_string_lossy()).expect("write server discovery");

        if let Err(error) = initialize_bundled_runtime() {
            eprintln!("warning: bundled runtime unavailable: {error}");
        }
        let state = match DaemonState::load() {
            Ok(state) => Arc::new(state),
            Err(error) => {
                eprintln!("dshboxd: cannot load state: {error}");
                std::process::exit(1);
            }
        };
        eprintln!(
            "dshboxd listening on {} (pid {})",
            endpoint.display(),
            std::process::id()
        );

        for stream in listener.incoming().flatten() {
            let token = discovery.token.clone();
            let state = state.clone();
            std::thread::spawn(move || handle(stream, state, &token));
        }
        remove_discovery();
    }

    fn handle(mut stream: UnixStream, state: Arc<DaemonState>, token: &str) {
        let mut reader = BufReader::new(stream.try_clone().expect("clone socket"));
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                let _ = writeln!(stream, r#"{{"ok":false,"error":"invalid json"}}"#);
                return;
            }
        };
        if request["token"].as_str() != Some(token) {
            let _ = writeln!(stream, r#"{{"ok":false,"error":"unauthorized"}}"#);
            return;
        }
        let response = dispatch::dispatch(&state, &request);
        let _ = writeln!(stream, "{response}");
        if dispatch::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            // A client asked for a clean stop (build-batch mismatch during
            // an upgrade): the response is already on the wire, so exit now.
            std::process::exit(0);
        }
    }
}

#[cfg(windows)]
mod windows {
    pub fn run() {
        eprintln!("dshboxd named-pipe transport is not yet implemented for Windows");
        std::process::exit(1);
    }
}

fn main() {
    #[cfg(unix)]
    unix::run();
    #[cfg(windows)]
    windows::run();
}
