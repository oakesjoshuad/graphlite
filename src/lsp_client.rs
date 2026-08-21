//! Minimal LSP client for rename operations.
//!
//! Spawns rust-analyzer over stdio, waits for it to become ready, then
//! exposes a single `rename` call that returns a `WorkspaceEdit` JSON value.
//! No external LSP crates are used — Content-Length framing is implemented
//! directly over the child process's stdin/stdout.

use std::{
    io::{BufRead, BufReader, BufWriter, IsTerminal, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

// ── Terminal progress ─────────────────────────────────────────────────────────

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Inline progress printer. Uses ANSI color + carriage-return overwrite on TTY;
/// falls back to plain `[ra] …` lines when stderr is not a terminal.
struct Progress {
    tty: bool,
    spin_idx: usize,
}

impl Progress {
    fn new() -> Self {
        Self { tty: std::io::stderr().is_terminal(), spin_idx: 0 }
    }

    /// Overwrite the current line with a spinner frame + message.
    fn update(&mut self, msg: &str) {
        self.spin_idx += 1;
        let s = SPINNER[self.spin_idx % SPINNER.len()];
        if self.tty {
            // \r  — return to line start
            // \x1b[K — erase to end of line (clears leftover chars from prev msg)
            eprint!("\r\x1b[K  \x1b[36m{s}\x1b[0m \x1b[1mrust-analyzer\x1b[0m  {msg}");
            let _ = std::io::stderr().flush();
        } else {
            debug!("{msg}");
        }
    }

    /// Print final "ready" line and move to the next line.
    fn ready(&self) {
        if self.tty {
            eprintln!("\r\x1b[K  \x1b[32m✓\x1b[0m \x1b[1mrust-analyzer\x1b[0m  ready");
        } else {
            info!("rust-analyzer ready");
        }
    }

    /// Print a warning (yellow) without clobbering the progress line.
    fn warn(&self, msg: &str) {
        if self.tty {
            eprintln!("\r\x1b[K  \x1b[33m!\x1b[0m {msg}");
        } else {
            warn!("{msg}");
        }
    }

    /// Print an error (red) and move to the next line.
    fn error(&self, msg: &str) {
        if self.tty {
            eprintln!("\r\x1b[K  \x1b[31m✗\x1b[0m {msg}");
        } else {
            tracing::error!("{msg}");
        }
    }
}

// ── Dependency check ──────────────────────────────────────────────────────────

/// Ensure `rust-analyzer` is available on PATH.
///
/// If missing, attempts installation via `rustup component add rust-analyzer`
/// after prompting the user for approval (when stdin is a TTY).
/// Falls back to a plain `cargo install rust-analyzer` suggestion when rustup
/// is not present.
pub fn ensure_rust_analyzer() -> Result<()> {
    if Command::new("rust-analyzer")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return Ok(());
    }

    let p = Progress::new();
    p.warn("rust-analyzer not found in PATH");

    let has_rustup = Command::new("rustup")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if has_rustup {
        let approved = if std::io::stdin().is_terminal() {
            eprint!("  \x1b[1mInstall via rustup component add rust-analyzer?\x1b[0m [Y/n] ");
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok();
            let answer = line.trim().to_lowercase();
            answer.is_empty() || answer == "y" || answer == "yes"
        } else {
            // Non-interactive: attempt install automatically.
            info!("stdin is not a TTY — attempting rustup install automatically");
            true
        };

        if !approved {
            bail!(
                "rust-analyzer is required for rename.\n\
                 Install with: rustup component add rust-analyzer"
            );
        }

        info!("installing rust-analyzer via rustup");
        eprintln!("  \x1b[36m…\x1b[0m installing rust-analyzer via rustup…");
        let status = Command::new("rustup")
            .args(["component", "add", "rust-analyzer"])
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run rustup: {}", e))?;

        if status.success() {
            info!("rust-analyzer installed");
            eprintln!("  \x1b[32m✓\x1b[0m rust-analyzer installed");
            Ok(())
        } else {
            bail!("rustup component add rust-analyzer failed — install manually and retry");
        }
    } else {
        bail!(
            "rust-analyzer not found and rustup is not available.\n\
             Install options:\n\
               rustup component add rust-analyzer\n\
               cargo install rust-analyzer\n\
             See: https://rust-analyzer.github.io"
        );
    }
}

// ── LspClient ─────────────────────────────────────────────────────────────────

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
    ///
    /// Calls `ensure_rust_analyzer()` first — installs via rustup on first run
    /// if missing and the user approves.
    pub fn connect(root: &str) -> Result<Self> {
        ensure_rust_analyzer()?;

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
            while let Ok(msg) = read_msg(&mut reader) {
                if msg.get("id").is_some() {
                    if response_tx.send(msg).is_err() {
                        break;
                    }
                } else if notif_tx.send(msg).is_err() {
                    break;
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

        let line_0 = line_1indexed.saturating_sub(1);
        let character = char_offset_of_name(file, line_1indexed, name);

        let abs_file = std::fs::canonicalize(file)
            .unwrap_or_else(|_| std::path::PathBuf::from(file));
        let uri = format!("file://{}", abs_file.display());

        // rust-analyzer requires textDocument/didOpen before rename; without it
        // the server returns error -32801 "content modified".
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

        // Retry on ContentModified (-32801): server may still be digesting didOpen.
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            let req_id = if attempts == 1 {
                id
            } else {
                let rid = self.next_id;
                self.next_id += 1;
                rid
            };

            write_msg(&mut self.writer, &json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "textDocument/rename",
                "params": {
                    "textDocument": {"uri": uri},
                    "position": {"line": line_0, "character": character},
                    "newName": new_name
                }
            }))?;

            let response = self.wait_for_id(req_id, Duration::from_secs(30))?;
            if let Some(error) = response.get("error") {
                if error.get("code").and_then(|c| c.as_i64()) == Some(-32801) && attempts < 5 {
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

        write_msg(&mut self.writer, &json!({
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
        }))?;
        self.wait_for_id(id, Duration::from_secs(60))?;

        write_msg(&mut self.writer, &json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))?;

        Ok(())
    }

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

// ── Readiness detection ───────────────────────────────────────────────────────

/// Wait until rust-analyzer is ready, printing inline progress to stderr.
///
/// Primary signal: `experimental/serverStatus` with `quiescent: true`.
/// Fallback: 2s of silence after all `$/progress` tokens have ended.
fn wait_for_ready(rx: &Receiver<Value>, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut active_progress: std::collections::HashSet<serde_json::Value> =
        std::collections::HashSet::new();
    let silence = Duration::from_secs(2);
    let mut prog = Progress::new();
    // Track the most recent title+message for display.
    let mut current_title = String::from("starting…");

    prog.update(&current_title);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            prog.error("timed out waiting for rust-analyzer to finish indexing");
            bail!("timeout waiting for rust-analyzer to become ready (indexing incomplete)");
        }

        let wait_for = if active_progress.is_empty() {
            silence.min(remaining)
        } else {
            remaining
        };

        match rx.recv_timeout(wait_for) {
            Err(_) if active_progress.is_empty() => {
                prog.ready();
                return Ok(());
            }
            Err(_) => {
                prog.error("rust-analyzer disconnected before becoming ready");
                bail!("rust-analyzer disconnected before becoming ready");
            }
            Ok(msg) => {
                let method = msg.get("method").and_then(|m| m.as_str());

                if method == Some("experimental/serverStatus") {
                    if msg["params"]["quiescent"].as_bool().unwrap_or(false) {
                        prog.ready();
                        return Ok(());
                    }
                    continue;
                }

                if method == Some("$/progress") {
                    let kind = msg["params"]["value"]["kind"].as_str().unwrap_or("");
                    let token = msg["params"]["token"].clone();

                    // Build a display string from title + optional message + percentage.
                    let title = msg["params"]["value"]["title"].as_str().unwrap_or("");
                    let message = msg["params"]["value"]["message"].as_str().unwrap_or("");
                    let pct = msg["params"]["value"]["percentage"].as_u64();

                    match kind {
                        "begin" => {
                            active_progress.insert(token);
                            current_title = title.to_string();
                        }
                        "report" => {
                            // Update title only when the server provides a new one.
                            if !title.is_empty() {
                                current_title = title.to_string();
                            }
                        }
                        "end" => {
                            active_progress.remove(&token);
                            if !title.is_empty() {
                                current_title = title.to_string();
                            }
                        }
                        _ => {}
                    }

                    // Compose the progress line.
                    let display = match (message.is_empty(), pct) {
                        (false, Some(p)) => format!("{current_title}  {message} ({p}%)"),
                        (false, None)    => format!("{current_title}  {message}"),
                        (true,  Some(p)) => format!("{current_title} ({p}%)"),
                        (true,  None)    => current_title.clone(),
                    };
                    prog.update(&display);
                }
            }
        }
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Find the UTF-8 byte offset of `name` as a whole-identifier token on the
/// given 1-indexed line of `file`.  Returns 0 as a safe fallback.
fn char_offset_of_name(file: &str, line_1indexed: u32, name: &str) -> u32 {
    let Ok(content) = std::fs::read_to_string(file) else { return 0; };
    let line_0 = (line_1indexed as usize).saturating_sub(1);
    let Some(line_text) = content.lines().nth(line_0) else { return 0; };

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
        if n == 0 { bail!("LSP connection closed"); }
        let trimmed = line.trim();
        if trimmed.is_empty() { break; }
        if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
            content_length = rest.trim().parse()?;
        }
    }
    if content_length == 0 { bail!("LSP message with zero content-length"); }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_offset_finds_name_after_fn_keyword() {
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "pub fn foobar() {}\n").unwrap();
        let offset = char_offset_of_name(path.to_str().unwrap(), 1, "foo");
        assert_eq!(offset, 0, "should not match 'foo' inside 'foobar'");
    }
}
