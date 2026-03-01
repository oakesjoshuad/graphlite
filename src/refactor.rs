use std::collections::HashMap;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::lsp;

struct TextEdit {
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
    new_text: String,
}

pub fn rename(symbol: &str, new_name: &str, root: &str) -> Result<()> {
    let conn = crate::query::open_db()?;

    // Resolve: integer id or sym:Name
    let (file, range_start, name): (String, i64, String) = if let Ok(id) = symbol.parse::<i64>() {
        conn.query_row(
            "SELECT file, range_start, name FROM nodes WHERE id = ?1",
            rusqlite::params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| anyhow!("node id {} not found", id))?
    } else {
        let name = symbol.strip_prefix("sym:").unwrap_or(symbol);
        conn.query_row(
            "SELECT file, range_start, name FROM nodes WHERE name = ?1 LIMIT 1",
            rusqlite::params![name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| anyhow!("symbol '{}' not found in database", name))?
    };

    let ra =
        lsp::which_rust_analyzer().ok_or_else(|| anyhow!("rust-analyzer not found in PATH"))?;

    let mut client = lsp::LspClient::spawn(&ra, &[], root)?;
    client.initialize()?;
    client.wait_for_ready(60)?;

    let abs_path = std::fs::canonicalize(&file)?;
    let uri = format!("file://{}", abs_path.display());
    let lsp_line = (range_start - 1).max(0) as u32;
    let char_offset = lsp::fn_name_char_offset(&file, range_start, &name);

    let workspace_edit = client.rename_symbol(&uri, lsp_line, char_offset, new_name)?;
    client.shutdown()?;

    let json_str = serde_json::to_string_pretty(&workspace_edit)?;
    std::fs::write("edits.json", &json_str)?;
    eprintln!("Wrote edits.json");
    Ok(())
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
            let path = lsp::uri_to_path(uri);
            let edits = parse_text_edits(&doc["edits"])?;
            result.entry(path).or_default().extend(edits);
        }
    }
    // Format 2: changes (object keyed by URI) — legacy
    else if let Some(changes) = edit["changes"].as_object() {
        for (uri, edits_val) in changes {
            let path = lsp::uri_to_path(uri);
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
