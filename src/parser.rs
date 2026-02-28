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
    #[allow(dead_code)]
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

// Maps tree-sitter node kind strings to our kind vocabulary.
// Called on the definition node (the @definition.* capture), not the name node.
fn kind_from_node(node: &Node) -> &'static str {
    match node.kind() {
        // Rust
        "function_item" => "fn",
        "struct_item" => "struct",
        "enum_item" => "enum",
        "union_item" => "union",
        "trait_item" => "trait",
        "type_item" => "type",
        "impl_item" => "impl",
        "mod_item" => "mod",
        "macro_definition" => "macro",
        // JS / TS shared
        "function_declaration" => "fn",
        "function_expression" => "fn",
        "function_signature" => "fn",
        "generator_function" => "fn",
        "generator_function_declaration" => "fn",
        "arrow_function" => "fn",
        "method_definition" => "fn",
        "method_signature" => "fn",
        "abstract_method_signature" => "fn",
        "assignment_expression" => "fn",
        "pair" => "fn",
        "variable_declarator" => "fn", // only reached via definition.function (arrow/fn value)
        "class_declaration" | "class" => "class",
        "abstract_class_declaration" => "class",
        // TS-specific
        "interface_declaration" => "interface",
        "type_alias_declaration" => "type",
        "module" => "module",
        // export wrappers - look at the value kind instead
        "lexical_declaration" | "variable_declaration" => "const",
        "export_statement" => "fn",
        _ => "unknown",
    }
}

fn extract_signature(symbol_node: &Node, source: &str) -> Option<String> {
    let source_bytes = source.as_bytes();

    let body_kinds = [
        "block",
        "statement_block",
        "declaration_block",
        "class_body",
    ];
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

    // Collect indices of all definition.* captures (definition.function,
    // definition.method, definition.class, definition.impl, etc.)
    let def_indices: Vec<u32> = query
        .capture_names()
        .iter()
        .enumerate()
        .filter(|(_, name)| name.starts_with("definition."))
        .map(|(i, _)| i as u32)
        .collect();

    let name_idx = match query.capture_index_for_name("name") {
        Some(i) => i,
        None => return Ok(vec![]),
    };

    if def_indices.is_empty() {
        return Ok(vec![]);
    }

    let matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

    let file_str = path.to_string_lossy().to_string();
    let lang_str = language.as_str().to_string();
    let mut symbols = Vec::new();

    for m in matches {
        let def_capture = m.captures.iter().find(|c| def_indices.contains(&c.index));
        let name_capture = m.captures.iter().find(|c| c.index == name_idx);

        if let (Some(def_cap), Some(name_cap)) = (def_capture, name_capture) {
            let symbol_node = def_cap.node;
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
                range_start: (symbol_node.start_position().row + 1) as u32,
                range_end: (symbol_node.end_position().row + 1) as u32,
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
        "function_expression",
        "arrow_function",
        "method_definition",
        "generator_function",
        "generator_function_declaration",
    ];
    let mut current = node.parent();
    while let Some(n) = current {
        if fn_kinds.contains(&n.kind()) {
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if child.kind() == "identifier"
                    || child.kind() == "property_identifier"
                    || child.kind() == "type_identifier"
                {
                    if let Ok(text) = child.utf8_text(source.as_bytes()) {
                        return Some(text.to_string());
                    }
                }
            }
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

    // Only CALLS edges. reference.class and reference.implementation are skipped.
    let ref_call_indices: Vec<u32> = query
        .capture_names()
        .iter()
        .enumerate()
        .filter(|(_, name)| **name == "reference.call")
        .map(|(i, _)| i as u32)
        .collect();

    let name_idx = match query.capture_index_for_name("name") {
        Some(i) => i,
        None => return Ok(vec![]),
    };

    if ref_call_indices.is_empty() {
        return Ok(vec![]);
    }

    let matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let file_str = path.to_string_lossy().to_string();
    let mut edges = Vec::new();

    for m in matches {
        let has_ref_call = m
            .captures
            .iter()
            .any(|c| ref_call_indices.contains(&c.index));
        if !has_ref_call {
            continue;
        }

        if let Some(name_cap) = m.captures.iter().find(|c| c.index == name_idx) {
            let callee_text = name_cap
                .node
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .to_string();
            if callee_text.is_empty() {
                continue;
            }

            let caller = enclosing_function_name(name_cap.node, source)
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
