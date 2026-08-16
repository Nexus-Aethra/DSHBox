//! Thin RPC helpers for the desktop app: every business operation goes
//! through the `dshboxd` daemon, mirroring the CLI's `cli/rpc.rs`. The
//! desktop keeps only presentation concerns (windows, read models, UI
//! preferences) local.

use box_client::RpcClient;
use serde_json::Value;

pub(crate) fn connect() -> Result<RpcClient, String> {
    RpcClient::connect()
}

pub(crate) fn call(client: &RpcClient, method: &str, params: Value) -> Result<Value, String> {
    client.call(method, params)
}

/// Resolve a possibly-relative path against the caller's working directory
/// before serializing it: the daemon's CWD differs from the caller's, so
/// relative paths would resolve against the wrong base.
pub(crate) fn absolutize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("github.com/")
        || std::path::Path::new(trimmed).is_absolute()
    {
        return trimmed.to_owned();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(trimmed).to_string_lossy().into_owned())
        .unwrap_or_else(|_| trimmed.to_owned())
}
