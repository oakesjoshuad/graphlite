use std::{fs, path::Path};

use anyhow::{anyhow, Result};
use tree_sitter::{Node, Parser, Query, QueryCursor};

use crate::language::{detect_language, Language};

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub file: String,
    pub language: String,
    pub kind: String,
    pub range_start: u32,
    pub range_end: u32,
    pub signature: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RawEdge {
    pub from_name: String,
    pub to_name: String,
    pub edge_type: String,
    pub file: String,
}

pub struct ParseResult {
    pub symbols: Vec<Symbol>,
    pub edges: Vec<RawEdge>,
}

pub fn parse_file(path: &Path) -> Result<ParseResult> {
    let language = detect_language(path).ok_or_else(|| anyhow!("unknown language"))?;
    let source = fs::read_to_string(path)?;
    let ts_lang = language.tree_sitter_language();

    let mut parser = Parser::new();
    parser.set_language(&ts_lang)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("parser returned None"))?;

    let query_src = language.query_source();
    // Trim comments to check if effectively empty
    let effective_src = query_src
        .lines()
        .filter(|l| !l.trim_start().starts_with(';') && !l.trim().is_empty())
        .count();

    if effective_src == 0 {
        return Ok(ParseResult {
            symbols: vec![],
            edges: vec![],
        });
    }

    let symbols = extract_symbols(&tree, &source, path, &language, &ts_lang, query_src)?;
    let edges = extract_edges(&tree, &source, path, &ts_lang, query_src)?;
    Ok(ParseResult { symbols, edges })
}

fn kind_from_node(node: &Node) -> &'static str {
    match node.kind() {
        "function_item" => "fn",
        "function_declaration" => "fn",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "trait_item" => "trait",
        "type_item" | "type_alias_declaration" => "type",
        "impl_item" => "impl",
        "interface_declaration" => "interface",
        "class_declaration" | "class" => "class",
        "lexical_declaration" => "const",
        "export_statement" => "fn",
        _ => "unknown",
    }
}

fn extract_signature(symbol_node: &Node, source: &str) -> Option<String> {
    let source_bytes = source.as_bytes();

    // Find the body child (block, statement_block, declaration_block, etc.)
    let body_kinds = ["block", "statement_block", "declaration_block", "class_body"];
    let mut body_start: Option<usize> = None;

    let mut cursor = symbol_node.walk();
    for child in symbol_node.children(&mut cursor) {
        if body_kinds.contains(&child.kind()) {
            body_start = Some(child.start_byte());
            break;
        }
    }

    let sig_end = body_start.unwrap_or(symbol_node.end_byte());
    let sig_bytes = &source_bytes[symbol_node.start_byte()..sig_end];
    let sig = std::str::from_utf8(sig_bytes).ok()?.trim().to_string();

    if sig.is_empty() {
        None
    } else {
        Some(sig)
    }
}

fn extract_symbols(
    tree: &tree_sitter::Tree,
    source: &str,
    path: &Path,
    language: &Language,
    ts_lang: &tree_sitter::Language,
    query_src: &str,
) -> Result<Vec<Symbol>> {
    let query = Query::new(ts_lang, query_src)?;
    let mut cursor = QueryCursor::new();

    let symbol_idx = query.capture_index_for_name("symbol");
    let name_idx = query.capture_index_for_name("name");

    if symbol_idx.is_none() || name_idx.is_none() {
        return Ok(vec![]);
    }
    let symbol_idx = symbol_idx.unwrap();
    let name_idx = name_idx.unwrap();

    let matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

    let file_str = path.to_string_lossy().to_string();
    let lang_str = language.as_str().to_string();
    let mut symbols = Vec::new();

    for m in matches {
        let symbol_capture = m.captures.iter().find(|c| c.index == symbol_idx);
        let name_capture = m.captures.iter().find(|c| c.index == name_idx);

        if let (Some(sym_cap), Some(name_cap)) = (symbol_capture, name_capture) {
            let symbol_node = sym_cap.node;
            let name_node = name_cap.node;

            let name = name_node
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }

            let kind = kind_from_node(&symbol_node).to_string();
            let signature = extract_signature(&symbol_node, source);

            symbols.push(Symbol {
                name,
                file: file_str.clone(),
                language: lang_str.clone(),
                kind,
                range_start: symbol_node.start_byte() as u32,
                range_end: symbol_node.end_byte() as u32,
                signature,
            });
        }
    }

    Ok(symbols)
}

fn enclosing_function_name<'a>(node: Node<'a>, source: &'a str) -> Option<String> {
    let fn_kinds = [
        "function_item",
        "function_declaration",
        "arrow_function",
        "method_definition",
    ];
    let mut current = node.parent();
    while let Some(n) = current {
        if fn_kinds.contains(&n.kind()) {
            // Try to find the name child
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if child.kind() == "identifier" || child.kind() == "type_identifier" {
                    if let Ok(text) = child.utf8_text(source.as_bytes()) {
                        return Some(text.to_string());
                    }
                }
            }
            // If we found a function but couldn't get its name, stop here
            return None;
        }
        current = n.parent();
    }
    None
}

fn extract_edges(
    tree: &tree_sitter::Tree,
    source: &str,
    path: &Path,
    ts_lang: &tree_sitter::Language,
    query_src: &str,
) -> Result<Vec<RawEdge>> {
    let query = Query::new(ts_lang, query_src)?;
    let mut cursor = QueryCursor::new();

    let call_idx = query.capture_index_for_name("call.target");
    if call_idx.is_none() {
        return Ok(vec![]);
    }
    let call_idx = call_idx.unwrap();

    let matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let file_str = path.to_string_lossy().to_string();
    let mut edges = Vec::new();

    for m in matches {
        for cap in m.captures.iter().filter(|c| c.index == call_idx) {
            let callee_text = cap
                .node
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .to_string();
            if callee_text.is_empty() {
                continue;
            }

            let caller = enclosing_function_name(cap.node, source)
                .unwrap_or_else(|| "<module>".to_string());

            edges.push(RawEdge {
                from_name: caller,
                to_name: callee_text,
                edge_type: "CALLS".to_string(),
                file: file_str.clone(),
            });
        }
    }

    Ok(edges)
}
