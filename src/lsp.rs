use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use log::{debug, warn};
use rusqlite::Connection;
use serde_json::{json, Value};

pub struct LspCallTarget {
    #[allow(dead_code)]
    pub name: String,
    pub uri: String,
    pub line: u32,
}

pub struct LspClient {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    rx: mpsc::Receiver<Value>,
    _reader: thread::JoinHandle<()>,
    next_id: u64,
    root: String,
    notification_buf: Vec<Value>,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn read_one_message(stdout: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        stdout.read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
            content_length = Some(val.trim().parse()?);
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("no Content-Length header in LSP message"))?;
    let mut buf = vec![0u8; len];
    stdout.read_exact(&mut buf)?;
    let msg: Value =
        serde_json::from_slice(&buf).map_err(|e| anyhow!("failed to parse LSP JSON: {}", e))?;

    // Debug: show every message received from the server
    let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("-");
    let id = msg
        .get("id")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "-".into());
    debug!("LSP[recv] method={} id={}", method, id);

    Ok(msg)
}

impl LspClient {
    pub fn spawn(server_cmd: &str, args: &[&str], root: &str) -> Result<Self> {
        let mut child = Command::new(server_cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| anyhow!("failed to spawn {}: {}", server_cmd, e))?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("no stdin on child process"))?,
        );
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("no stdout on child process"))?;

        let (tx, rx) = mpsc::channel::<Value>();
        let _reader = thread::spawn(move || {
            let mut out = BufReader::new(child_stdout);
            while let Ok(msg) = read_one_message(&mut out) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });

        Ok(LspClient {
            child,
            stdin,
            rx,
            _reader,
            next_id: 1,
            root: root.to_string(),
            notification_buf: Vec::new(),
        })
    }

    fn send_raw(&mut self, msg: &Value) -> Result<()> {
        let body = serde_json::to_string(msg)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn send_request(&mut self, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_raw(&msg)?;
        Ok(id)
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_raw(&msg)
    }

    fn read_message_timeout(&mut self, dur: Duration) -> Option<Value> {
        self.rx.recv_timeout(dur).ok()
    }

    // Send a request and wait for the matching response.
    // Messages are classified by whether they have a `method` field:
    //   - has `method` + `id`  → server-to-client request; acknowledge with null result
    //   - has `method`, no `id` → notification; buffer for wait_for_ready
    //   - no `method`, matching `id` → our response; return it
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send_request(method, params)?;
        loop {
            let msg = match self.read_message_timeout(Duration::from_secs(10)) {
                Some(m) => m,
                None => return Err(anyhow!("timeout waiting for response to '{}'", method)),
            };

            if msg.get("method").is_some() {
                if let Some(server_req_id) = msg.get("id") {
                    // Server-to-client request (e.g. window/workDoneProgress/create).
                    // Must acknowledge or the server may stall waiting for our reply.
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": server_req_id,
                        "result": null
                    });
                    let _ = self.send_raw(&reply);
                } else {
                    // Notification — buffer for wait_for_ready
                    self.notification_buf.push(msg);
                }
                continue;
            }

            // No `method` → it's a response. Match by id.
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(anyhow!("LSP error for '{}': {}", method, err));
                }
                return Ok(msg["result"].clone());
            }
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        let abs_root = std::fs::canonicalize(&self.root)
            .unwrap_or_else(|_| std::path::PathBuf::from(&self.root));
        let root_uri = format!("file://{}", abs_root.display());

        let _result = self.call(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "callHierarchy": {
                            "dynamicRegistration": false
                        }
                    },
                    "workspace": {
                        "workspaceEdit": {
                            "documentChanges": true
                        }
                    },
                    "experimental": {
                        "serverStatusNotification": true
                    }
                }
            }),
        )?;

        self.send_notification("initialized", json!({}))?;
        Ok(())
    }

    // Poll for experimental/serverStatus quiescent notification. Drains buffered
    // notifications first (the signal often arrives during initialize()). Uses
    // recv_timeout for a true deadline — no blocking reads can prevent the timeout.
    pub fn wait_for_ready(&mut self, timeout_secs: u64) -> Result<()> {
        let buffered = std::mem::take(&mut self.notification_buf);
        for msg in buffered {
            if is_quiescent(&msg) {
                return Ok(());
            }
        }

        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                debug!("LSP: timeout waiting for ready, proceeding");
                return Ok(());
            }
            match self.read_message_timeout(remaining) {
                Some(msg) => {
                    if is_quiescent(&msg) {
                        debug!("LSP: ready (quiescent)");
                        return Ok(());
                    }
                    // Buffer non-quiescent notifications; ignore responses
                    if msg.get("id").is_none() {
                        self.notification_buf.push(msg);
                    }
                }
                None => {
                    debug!("LSP: timeout waiting for ready, proceeding");
                    return Ok(());
                }
            }
        }
    }

    pub fn outgoing_calls(
        &mut self,
        uri: &str,
        line: u32,
        char_offset: u32,
    ) -> Result<Vec<LspCallTarget>> {
        let items = self.call(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": char_offset }
            }),
        )?;

        let items = match items {
            Value::Array(arr) if !arr.is_empty() => arr,
            _ => return Ok(vec![]),
        };

        let mut targets = Vec::new();
        for item in &items {
            let calls = match self.call("callHierarchy/outgoingCalls", json!({ "item": item })) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if let Some(calls_arr) = calls.as_array() {
                for call in calls_arr {
                    let to_item = &call["to"];
                    let name = to_item["name"].as_str().unwrap_or("").to_string();
                    let uri = to_item["uri"].as_str().unwrap_or("").to_string();
                    // Use range.start.line (0-indexed) to match against DB range_start - 1
                    let target_line =
                        to_item["range"]["start"]["line"].as_u64().unwrap_or(0) as u32;
                    if !uri.is_empty() && !name.is_empty() {
                        targets.push(LspCallTarget {
                            name,
                            uri,
                            line: target_line,
                        });
                    }
                }
            }
        }

        Ok(targets)
    }

    pub fn rename_symbol(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<Value> {
        let prep = self.call(
            "textDocument/prepareRename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character }
            }),
        )?;
        if prep.is_null() {
            return Err(anyhow!(
                "rust-analyzer: symbol is not renameable at this position"
            ));
        }
        self.call(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
                "newName": new_name
            }),
        )
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let _ = self.call("shutdown", json!(null));
        let _ = self.send_notification("exit", json!(null));
        Ok(())
    }
}

fn is_quiescent(msg: &Value) -> bool {
    msg.get("method").and_then(|v| v.as_str()) == Some("experimental/serverStatus")
        && msg["params"]["quiescent"].as_bool() == Some(true)
}

pub(crate) fn which_rust_analyzer() -> Option<String> {
    which_server("rust-analyzer")
}

pub(crate) fn which_typescript_language_server() -> Option<String> {
    which_server("typescript-language-server")
}

fn which_server(name: &str) -> Option<String> {
    Command::new("which")
        .arg(name)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

// Find the character offset of `name` on the given 1-indexed source line.
// Used to point prepareCallHierarchy at the function name rather than the
// `fn` keyword; rust-analyzer uses the returned selectionRange to re-identify
// the function in callHierarchy/outgoingCalls.
pub(crate) fn fn_name_char_offset(file: &str, range_start: i64, name: &str) -> u32 {
    let line_idx = (range_start - 1).max(0) as usize;
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(_) => return 3,
    };
    if let Some(line) = content.lines().nth(line_idx) {
        let nb = name.as_bytes();
        let lb = line.as_bytes();
        let mut i = 0usize;
        while i + nb.len() <= lb.len() {
            if &lb[i..i + nb.len()] == nb {
                let before_ok = i == 0 || {
                    let c = lb[i - 1] as char;
                    !c.is_alphanumeric() && c != '_'
                };
                let after_ok = i + nb.len() >= lb.len() || {
                    let c = lb[i + nb.len()] as char;
                    !c.is_alphanumeric() && c != '_'
                };
                if before_ok && after_ok {
                    return i as u32;
                }
            }
            i += 1;
        }
    }
    3 // fallback: `fn name` positions name at char 3
}

pub fn enrich(conn: &Connection, root: &str, language: &str) -> Result<()> {
    match language {
        "rust" => enrich_rust(conn, root),
        "typescript" | "javascript" => enrich_typescript(conn, root, language),
        _ => {
            warn!("LSP: no enrichment support for '{}', skipping", language);
            Ok(())
        }
    }
}

fn enrich_rust(conn: &Connection, root: &str) -> Result<()> {
    let ra = match which_rust_analyzer() {
        Some(path) => path,
        None => {
            warn!("LSP: rust-analyzer not found in PATH, skipping enrichment");
            return Ok(());
        }
    };

    let mut client = LspClient::spawn(&ra, &[], root)?;
    client.initialize()?;
    client.wait_for_ready(60)?;

    // Build (canonical_path, range_start) -> node_id index for fast target lookup
    let mut path_line_to_id: HashMap<(String, i64), i64> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, file, range_start FROM nodes")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, file, range_start) = row?;
            let canonical = std::fs::canonicalize(&file)
                .map(|p| p.display().to_string())
                .unwrap_or(file);
            path_line_to_id.insert((canonical, range_start), id);
        }
    }

    // Query all fn nodes to drive call hierarchy requests
    let fn_nodes: Vec<(i64, String, i64, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, file, range_start, name FROM nodes WHERE kind = 'fn'")?;
        let result = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result
    };

    // Collect trusted CALLS_TRUSTED edges via LSP call hierarchy
    let mut trusted_edges: Vec<(i64, i64, &'static str)> = Vec::new();

    for (fn_id, file, range_start, name) in &fn_nodes {
        let abs_path =
            std::fs::canonicalize(file).unwrap_or_else(|_| std::path::PathBuf::from(file));
        let uri = format!("file://{}", abs_path.display());
        // LSP uses 0-indexed lines; range_start is 1-indexed
        let lsp_line = (*range_start - 1).max(0) as u32;
        // Point at the function name, not the `fn` keyword. rust-analyzer builds
        // selectionRange from this position, then uses it to re-identify the
        // function in callHierarchy/outgoingCalls. Pointing at `fn` (char 0)
        // produces selectionRange covering "fn" and outgoingCalls returns [].
        let char_offset = fn_name_char_offset(file, *range_start, name);

        let targets = match client.outgoing_calls(&uri, lsp_line, char_offset) {
            Ok(t) => t,
            Err(_) => continue,
        };

        for target in targets {
            let target_path = uri_to_path(&target.uri);
            // LSP line is 0-indexed; range_start in DB is 1-indexed
            let target_range_start = target.line as i64 + 1;
            if let Some(&to_id) = path_line_to_id.get(&(target_path, target_range_start)) {
                trusted_edges.push((*fn_id, to_id, "CALLS_TRUSTED"));
            }
        }
    }

    // Collect TRAIT_IMPL edges via name matching
    {
        let mut stmt = conn.prepare(
            "SELECT impl.id, trait_node.id
             FROM nodes impl
             JOIN nodes trait_node ON impl.name = trait_node.name
             WHERE impl.kind = 'impl' AND trait_node.kind = 'trait'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (impl_id, trait_id) = row?;
            trusted_edges.push((impl_id, trait_id, "TRAIT_IMPL"));
        }
    }

    client.shutdown()?;

    let edge_count = trusted_edges.len();
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO edges (from_id, to_id, edge_type, source, confidence)
             VALUES (?1, ?2, ?3, 'rust-analyzer', 1.0)",
        )?;
        for (from_id, to_id, edge_type) in &trusted_edges {
            stmt.execute(rusqlite::params![from_id, to_id, edge_type])?;
        }
    }
    tx.commit()?;

    eprintln!("LSP: inserted {} trusted edges (rust-analyzer)", edge_count);
    Ok(())
}

fn enrich_typescript(conn: &Connection, root: &str, language: &str) -> Result<()> {
    let ts = match which_typescript_language_server() {
        Some(path) => path,
        None => {
            warn!("LSP: typescript-language-server not found in PATH, skipping enrichment");
            return Ok(());
        }
    };

    let mut client = LspClient::spawn(&ts, &["--stdio"], root)?;
    client.initialize()?;
    // typescript-language-server has no quiescent signal; wait up to 30 s then proceed
    client.wait_for_ready(30)?;

    // Build (canonical_path, range_start) -> node_id index
    let mut path_line_to_id: HashMap<(String, i64), i64> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, file, range_start FROM nodes")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, file, range_start) = row?;
            let canonical = std::fs::canonicalize(&file)
                .map(|p| p.display().to_string())
                .unwrap_or(file);
            path_line_to_id.insert((canonical, range_start), id);
        }
    }

    // Query fn nodes for the target language(s)
    let lang_filter = match language {
        "javascript" => "'javascript'",
        _ => "'typescript', 'javascript'",
    };
    let sql = format!(
        "SELECT id, file, range_start, name FROM nodes WHERE kind = 'fn' AND language IN ({})",
        lang_filter
    );
    let fn_nodes: Vec<(i64, String, i64, String)> = {
        let mut stmt = conn.prepare(&sql)?;
        let result = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result
    };

    let mut trusted_edges: Vec<(i64, i64)> = Vec::new();

    for (fn_id, file, range_start, name) in &fn_nodes {
        let abs_path =
            std::fs::canonicalize(file).unwrap_or_else(|_| std::path::PathBuf::from(file));
        let uri = format!("file://{}", abs_path.display());
        let lsp_line = (*range_start - 1).max(0) as u32;
        let char_offset = fn_name_char_offset(file, *range_start, name);

        let targets = match client.outgoing_calls(&uri, lsp_line, char_offset) {
            Ok(t) => t,
            Err(_) => continue,
        };

        for target in targets {
            let target_path = uri_to_path(&target.uri);
            let target_range_start = target.line as i64 + 1;
            if let Some(&to_id) = path_line_to_id.get(&(target_path, target_range_start)) {
                trusted_edges.push((*fn_id, to_id));
            }
        }
    }

    client.shutdown()?;

    let edge_count = trusted_edges.len();
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO edges (from_id, to_id, edge_type, source, confidence)
             VALUES (?1, ?2, 'CALLS_TRUSTED', 'typescript-language-server', 1.0)",
        )?;
        for (from_id, to_id) in &trusted_edges {
            stmt.execute(rusqlite::params![from_id, to_id])?;
        }
    }
    tx.commit()?;

    eprintln!("LSP: inserted {} trusted edges (typescript-language-server)", edge_count);
    Ok(())
}
