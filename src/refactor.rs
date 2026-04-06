use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;

struct TextEdit {
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
    new_text: String,
}

/// Rename a symbol via the running `graphlite watch` daemon.
///
/// Sends a `Rename` IPC message to the watcher, which uses its lazily-initialized
/// rust-analyzer LSP client to produce a `WorkspaceEdit`. The edit is written to
/// `edits.json` in the current directory. Preview with `graphlite diff-rename`,
/// apply atomically with `graphlite apply-edits`.
///
/// Requires `graphlite watch <root>` to be running — the watcher keeps
/// rust-analyzer warm so subsequent renames have zero startup cost.
pub fn rename(symbol: &str, new_name: &str, root: &str) -> Result<()> {
    if !crate::ipc::is_watcher_running(root) {
        anyhow::bail!(
            "rename requires an active watcher.\n\
             Start it with:  graphlite watch {}\n\
             Then retry:     graphlite rename {} {}",
            root,
            symbol,
            new_name
        );
    }

    eprintln!("[rename] requesting rename of '{}' -> '{}'", symbol, new_name);
    eprintln!("[rename] (first call warms rust-analyzer; subsequent calls are fast)");

    let response = crate::ipc::send_msg_timeout(
        root,
        &crate::ipc::WatchMsg::Rename {
            symbol: symbol.to_string(),
            new_name: new_name.to_string(),
        },
        Duration::from_secs(120),
    )?;

    if !response.ok {
        anyhow::bail!(
            "{}",
            response.error.unwrap_or_else(|| "rename failed".into())
        );
    }

    let edit_json = response
        .data
        .ok_or_else(|| anyhow!("watcher returned no edit data"))?;

    std::fs::write("edits.json", &edit_json)?;

    let edit: Value = serde_json::from_str(&edit_json)?;
    let file_count = count_affected_files(&edit);
    eprintln!(
        "[rename] '{}' -> '{}': {} file(s) affected",
        symbol, new_name, file_count
    );
    eprintln!("[rename] edits written to edits.json");
    eprintln!("[rename] review:  graphlite diff-rename");
    eprintln!("[rename] apply:   graphlite apply-edits");

    Ok(())
}

fn count_affected_files(edit: &Value) -> usize {
    if let Some(arr) = edit["documentChanges"].as_array() {
        return arr.len();
    }
    if let Some(obj) = edit["changes"].as_object() {
        return obj.len();
    }
    0
}

pub fn diff_rename(edits_file: &str) -> Result<()> {
    let raw = std::fs::read_to_string(edits_file)?;
    let edit: Value = serde_json::from_str(&raw)?;
    let file_edits = parse_workspace_edit(&edit)?;

    for (path, text_edits) in &file_edits {
        let content = std::fs::read_to_string(path)?;
        let lines: Vec<&str> = content.lines().collect();
        for te in text_edits {
            let start_byte = utf16_to_byte_offset(&lines, te.start_line, te.start_char);
            let end_byte = utf16_to_byte_offset(&lines, te.end_line, te.end_char);
            let old_text = &content[start_byte..end_byte];
            println!(
                "{} L{}:{}: {:?} -> {:?}",
                path,
                te.start_line + 1,
                te.start_char,
                old_text,
                te.new_text
            );
        }
    }
    Ok(())
}

pub fn apply_edits(edits_file: &str) -> Result<()> {
    let raw = std::fs::read_to_string(edits_file)?;
    let edit: Value = serde_json::from_str(&raw)?;
    let file_edits = parse_workspace_edit(&edit)?;

    // PHASE 1: apply all edits in memory — no disk writes yet
    let mut staged: Vec<(String, String)> = Vec::new();
    for (path, mut text_edits) in file_edits {
        let content = std::fs::read_to_string(&path)?;
        // Sort DESCENDING by position to avoid index drift when applying
        text_edits.sort_by(|a, b| {
            b.start_line
                .cmp(&a.start_line)
                .then(b.start_char.cmp(&a.start_char))
        });
        let new_content = apply_text_edits(&content, &text_edits)?;
        staged.push((path, new_content));
    }

    // PHASE 2: atomic writes — write to .tmp then rename
    for (path, new_content) in staged {
        let tmp = format!("{}.tmp", path);
        std::fs::write(&tmp, &new_content)?;
        std::fs::rename(&tmp, &path)?;
        eprintln!("Applied: {}", path);
    }
    Ok(())
}

fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String> {
    // edits must already be sorted descending by position
    let lines: Vec<&str> = content.lines().collect();
    let mut result = content.to_string();
    for edit in edits {
        let start = utf16_to_byte_offset(&lines, edit.start_line, edit.start_char);
        let end = utf16_to_byte_offset(&lines, edit.end_line, edit.end_char);
        result.replace_range(start..end, &edit.new_text);
    }
    Ok(result)
}

fn utf16_to_byte_offset(lines: &[&str], line: u32, utf16_char: u32) -> usize {
    let line_idx = line as usize;
    let line_start: usize = lines[..line_idx].iter().map(|l| l.len() + 1).sum();
    let line_str = if line_idx < lines.len() {
        lines[line_idx]
    } else {
        ""
    };
    let mut utf16_so_far = 0u32;
    let mut byte_pos = 0usize;
    for ch in line_str.chars() {
        if utf16_so_far >= utf16_char {
            break;
        }
        utf16_so_far += ch.len_utf16() as u32;
        byte_pos += ch.len_utf8();
    }
    line_start + byte_pos
}

fn parse_workspace_edit(edit: &Value) -> Result<Vec<(String, Vec<TextEdit>)>> {
    let mut result: HashMap<String, Vec<TextEdit>> = HashMap::new();

    // Format 1: documentChanges (array) — preferred by rust-analyzer
    if let Some(changes) = edit["documentChanges"].as_array() {
        for doc in changes {
            let uri = doc["textDocument"]["uri"]
                .as_str()
                .ok_or_else(|| anyhow!("missing uri in documentChanges"))?;
            let path = uri_to_path(uri);
            let edits = parse_text_edits(&doc["edits"])?;
            result.entry(path).or_default().extend(edits);
        }
    }
    // Format 2: changes (object keyed by URI) — legacy
    else if let Some(changes) = edit["changes"].as_object() {
        for (uri, edits_val) in changes {
            let path = uri_to_path(uri);
            let edits = parse_text_edits(edits_val)?;
            result.entry(path).or_default().extend(edits);
        }
    } else {
        return Err(anyhow!(
            "edits.json has neither documentChanges nor changes field"
        ));
    }

    Ok(result.into_iter().collect())
}

fn parse_text_edits(val: &Value) -> Result<Vec<TextEdit>> {
    val.as_array()
        .ok_or_else(|| anyhow!("edits is not an array"))?
        .iter()
        .map(|e| {
            Ok(TextEdit {
                start_line: e["range"]["start"]["line"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("missing start.line"))?
                    as u32,
                start_char: e["range"]["start"]["character"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("missing start.character"))?
                    as u32,
                end_line: e["range"]["end"]["line"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("missing end.line"))? as u32,
                end_char: e["range"]["end"]["character"]
                    .as_u64()
                    .ok_or_else(|| anyhow!("missing end.character"))?
                    as u32,
                new_text: e["newText"]
                    .as_str()
                    .ok_or_else(|| anyhow!("missing newText"))?
                    .to_string(),
            })
        })
        .collect()
}

fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}
