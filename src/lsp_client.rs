//! Minimal LSP client for rename operations.
//!
//! Spawns rust-analyzer over stdio, waits for it to become ready, then
//! exposes a single `rename` call that returns a `WorkspaceEdit` JSON value.
//! No external LSP crates are used — Content-Length framing is implemented
//! directly over the child process's stdin/stdout.

use std::{
    io::{BufRead, BufReader, BufWriter, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// Minimal LSP client that connects to rust-analyzer and supports rename.
///
/// The reader thread routes messages to two channels:
/// - `response_rx`: messages with an `id` field (responses to requests)
/// - `notif_rx`: messages without `id` (server-initiated notifications)
///
/// This ensures notifications sent during `initialize` are not accidentally
/// discarded while waiting for the initialize response.
pub struct LspClient {
    writer: BufWriter<ChildStdin>,
    /// Receives JSON-RPC responses (messages containing `"id"`).
    response_rx: Receiver<Value>,
    /// Receives JSON-RPC notifications (messages without `"id"`).
    notif_rx: Receiver<Value>,
    _reader_thread: thread::JoinHandle<()>,
    child: Child,
    next_id: u64,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = send_shutdown(&mut self.writer, self.next_id);
        let _ = self.child.wait();
    }
}

impl LspClient {
    /// Spawn rust-analyzer in `root`, send `initialize`, and wait for the server
    /// to report `quiescent: true` via `experimental/serverStatus`.
    pub fn connect(root: &str) -> Result<Self> {
        let abs_root = std::fs::canonicalize(root)
            .unwrap_or_else(|_| std::path::PathBuf::from(root));

        let mut child = Command::new("rust-analyzer")
            .current_dir(&abs_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn rust-analyzer: {}", e))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let (response_tx, response_rx) = mpsc::channel::<Value>();
        let (notif_tx, notif_rx) = mpsc::channel::<Value>();

        let reader_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_msg(&mut reader) {
                    Ok(msg) => {
                        // Route by presence of "id": responses vs notifications.
                        if msg.get("id").is_some() {
                            if response_tx.send(msg).is_err() { break; }
                        } else {
                            if notif_tx.send(msg).is_err() { break; }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut client = LspClient {
            writer: BufWriter::new(stdin),
            response_rx,
            notif_rx,
            _reader_thread: reader_thread,
            child,
            next_id: 1,
        };

        client.do_initialize(&abs_root.to_string_lossy())?;
        wait_for_ready(&client.notif_rx, Duration::from_secs(120))?;

        Ok(client)
    }

    /// Rename the symbol whose declaration starts at `line_1indexed` in `file`.
    ///
    /// Returns the raw `WorkspaceEdit` JSON value from rust-analyzer.
    pub fn rename(
        &mut self,
        file: &str,
        line_1indexed: u32,
        name: &str,
        new_name: &str,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        // LSP uses 0-indexed lines and characters.
        let line_0 = line_1indexed.saturating_sub(1);
        let character = char_offset_of_name(file, line_1indexed, name);

        let abs_file = std::fs::canonicalize(file)
            .unwrap_or_else(|_| std::path::PathBuf::from(file));
        let uri = format!("file://{}", abs_file.display());

        // rust-analyzer requires textDocument/didOpen before rename; without it
        // the server returns error -32801 "content modified" because it has no
        // in-memory copy of the document.
        let text = std::fs::read_to_string(&abs_file).unwrap_or_default();
        write_msg(&mut self.writer, &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "rust",
                    "version": 1,
                    "text": text
                }
            }
        }))?;

        // Retry on ContentModified (-32801): rust-analyzer may still be
        // digesting the didOpen when the rename request arrives.
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            let req_id = if attempts == 1 { id } else {
                // Reuse same logical id on first attempt; bump for retries so
                // we don't pick up a stale response from the first request.
                let rid = self.next_id;
                self.next_id += 1;
                rid
            };

            let req = json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "textDocument/rename",
                "params": {
                    "textDocument": {"uri": uri},
                    "position": {"line": line_0, "character": character},
                    "newName": new_name
                }
            });

            write_msg(&mut self.writer, &req)?;

            let response = self.wait_for_id(req_id, Duration::from_secs(30))?;
            if let Some(error) = response.get("error") {
                // -32801 = ContentModified: server re-analyzed while we waited.
                // Retry with backoff up to 5 times (covers ~15 s of indexing).
                if error.get("code").and_then(|c| c.as_i64()) == Some(-32801)
                    && attempts < 5
                {
                    std::thread::sleep(Duration::from_millis(500 * u64::from(attempts)));
                    continue;
                }
                bail!("rust-analyzer rename error: {}", error);
            }
            let result = response["result"].clone();
            if result.is_null() {
                bail!("rust-analyzer returned null result — position may not resolve to a renameable symbol");
            }
            return Ok(result);
        }
    }

    fn do_initialize(&mut self, root_path: &str) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;

        let root_uri = format!("file://{}", root_path);
        let pid = std::process::id();

        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": pid,
                "rootUri": root_uri,
                "capabilities": {
                    "workspace": {},
                    "textDocument": {
                        "rename": {
                            "dynamicRegistration": false,
                            "prepareSupport": false
                        }
                    }
                },
                "workspaceFolders": [{"uri": root_uri, "name": "workspace"}]
            }
        });

        write_msg(&mut self.writer, &req)?;
        self.wait_for_id(id, Duration::from_secs(60))?;

        write_msg(&mut self.writer, &json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))?;

        Ok(())
    }

    /// Wait for the response with the given `id` on the response channel.
    /// Since responses and notifications are routed to separate channels,
    /// this cannot accidentally consume `experimental/serverStatus` or progress
    /// notifications needed by `wait_for_ready`.
    fn wait_for_id(&self, id: u64, timeout: Duration) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timeout waiting for LSP response id={}", id);
            }
            let msg = self
                .response_rx
                .recv_timeout(remaining)
                .map_err(|_| anyhow::anyhow!("rust-analyzer disconnected waiting for id={}", id))?;
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return Ok(msg);
            }
        }
    }
}

/// Wait until rust-analyzer is ready.
///
/// Primary signal: `experimental/serverStatus` with `quiescent: true`.
/// Fallback: after all `$/progress` items have ended, accept 2s of silence
/// (handles rust-analyzer builds that don't emit `experimental/serverStatus`).
fn wait_for_ready(rx: &Receiver<Value>, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut active_progress: std::collections::HashSet<serde_json::Value> =
        std::collections::HashSet::new();
    let silence = Duration::from_secs(2);

    loop {
        // Compute how long we're willing to wait for the next message.
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timeout waiting for rust-analyzer to become ready (indexing incomplete)");
        }
        // If no progress items are active, accept silence as readiness.
        let wait_for = if active_progress.is_empty() {
            silence.min(remaining)
        } else {
            remaining
        };

        match rx.recv_timeout(wait_for) {
            Err(_) if active_progress.is_empty() => {
                // Silence after all progress ended — server is idle.
                return Ok(());
            }
            Err(_) => {
                bail!("rust-analyzer disconnected before becoming ready");
            }
            Ok(msg) => {
                let method = msg.get("method").and_then(|m| m.as_str());

                if method == Some("experimental/serverStatus") {
                    if msg["params"]["quiescent"].as_bool().unwrap_or(false) {
                        return Ok(());
                    }
                    continue;
                }

                if method == Some("$/progress") {
                    let kind = msg["params"]["value"]["kind"].as_str().unwrap_or("");
                    let token = msg["params"]["token"].clone();
                    match kind {
                        "begin" => { active_progress.insert(token); }
                        "end"   => { active_progress.remove(&token); }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Find the UTF-8 byte offset of `name` as a whole-identifier token on the
/// given 1-indexed line of `file`.  Returns 0 as a safe fallback so that
/// rust-analyzer still attempts the rename at the line start.
fn char_offset_of_name(file: &str, line_1indexed: u32, name: &str) -> u32 {
    let Ok(content) = std::fs::read_to_string(file) else {
        return 0;
    };
    let line_0 = (line_1indexed as usize).saturating_sub(1);
    let Some(line_text) = content.lines().nth(line_0) else {
        return 0;
    };

    let bytes = line_text.as_bytes();
    let name_bytes = name.as_bytes();

    let mut i = 0usize;
    while i + name_bytes.len() <= bytes.len() {
        if &bytes[i..i + name_bytes.len()] == name_bytes {
            let before_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after_pos = i + name_bytes.len();
            let after_ok = after_pos >= bytes.len()
                || !(bytes[after_pos].is_ascii_alphanumeric() || bytes[after_pos] == b'_');
            if before_ok && after_ok {
                return i as u32;
            }
        }
        i += 1;
    }
    0
}

fn send_shutdown(writer: &mut BufWriter<ChildStdin>, id: u64) -> Result<()> {
    write_msg(writer, &json!({"jsonrpc":"2.0","id":id,"method":"shutdown","params":null}))?;
    let _ = write_msg(writer, &json!({"jsonrpc":"2.0","method":"exit","params":null}));
    Ok(())
}

fn write_msg(writer: &mut BufWriter<ChildStdin>, value: &Value) -> Result<()> {
    let body = serde_json::to_string(value)?;
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()?;
    Ok(())
}

fn read_msg(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            bail!("LSP connection closed");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
            content_length = rest.trim().parse()?;
        }
    }
    if content_length == 0 {
        bail!("LSP message with zero content-length");
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_offset_finds_name_after_fn_keyword() {
        // Write a temp file with a known function signature.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "pub fn my_function(x: u32) -> u32 { x }\n").unwrap();
        let offset = char_offset_of_name(path.to_str().unwrap(), 1, "my_function");
        assert_eq!(offset, 7, "my_function starts at byte 7 after 'pub fn '");
    }

    #[test]
    fn char_offset_returns_zero_for_missing_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "pub fn other_function() {}\n").unwrap();
        let offset = char_offset_of_name(path.to_str().unwrap(), 1, "missing_name");
        assert_eq!(offset, 0);
    }

    #[test]
    fn char_offset_respects_word_boundaries() {
        // "foo" must not match inside "foobar"
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "pub fn foobar() {}\n").unwrap();
        let offset = char_offset_of_name(path.to_str().unwrap(), 1, "foo");
        assert_eq!(offset, 0, "should not match 'foo' inside 'foobar'");
    }
}
