use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WatchMsg {
    Ping,
    Annotate {
        symbol: String,
        intent: Option<String>,
        behavior: Option<String>,
        tags: Option<String>,
        source: String,
        confidence: f64,
    },
    Reindex {
        file: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct WatchResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn sock_path(root: &str) -> PathBuf {
    PathBuf::from(root).join(".graphlite/watcher.sock")
}

/// Send a message to the watcher and return its response.
/// Uses a 2-second timeout on both read and write.
pub fn send_msg(root: &str, msg: &WatchMsg) -> Result<WatchResponse> {
    let path = sock_path(root);
    let stream = UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let mut write_stream = stream.try_clone()?;
    let line = serde_json::to_string(msg)? + "\n";
    write_stream.write_all(line.as_bytes())?;

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.read_line(&mut response_line)?;
    let response: WatchResponse = serde_json::from_str(response_line.trim())?;
    Ok(response)
}

/// Returns true if a watcher process is listening on the socket and responds to Ping.
pub fn is_watcher_running(root: &str) -> bool {
    send_msg(root, &WatchMsg::Ping).is_ok()
}
