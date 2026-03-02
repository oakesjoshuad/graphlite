use std::{fs, path::Path};

use anyhow::{anyhow, Result};
use streaming_iterator::StreamingIterator;
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
    pub content_hash: String,
    pub visibility: String,
    pub doc: Option<String>,
    pub stable_id: String,
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
    pub file_doc: Option<String>,
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

    let file_doc = extract_file_doc(&source, &language);

    if effective_src == 0 {
        return Ok(ParseResult {
            symbols: vec![],
            edges: vec![],
            file_doc,
        });
    }

    let mut symbols = extract_symbols(&tree, &source, path, &language, &ts_lang, query_src)?;
    let mut edges = extract_edges(&tree, &source, path, &ts_lang, query_src)?;

    if matches!(language, Language::Svelte) {
        let (script_symbols, script_edges) = extract_svelte_script_symbols(&tree, &source, path)?;
        symbols.extend(script_symbols);
        edges.extend(script_edges);
        let mut seen = std::collections::HashSet::new();
        symbols.retain(|s| seen.insert((s.file.clone(), s.range_start)));
    }

    Ok(ParseResult {
        symbols,
        edges,
        file_doc,
    })
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

    // Pass 1: body as a direct child (function_declaration, method_definition, etc.)
    let mut body_start: Option<usize> = None;
    let mut cursor = symbol_node.walk();
    for child in symbol_node.children(&mut cursor) {
        if body_kinds.contains(&child.kind()) {
            body_start = Some(child.start_byte());
            break;
        }
    }

    // Pass 2: body nested one level deeper inside an arrow_function or
    // function_expression child. This covers variable_declarator, assignment_expression,
    // and pair nodes where the value is `(...) => { body }` — the statement_block
    // is a grandchild, invisible to pass 1.
    if body_start.is_none() {
        let fn_wrapper_kinds = [
            "arrow_function",
            "function_expression",
            "generator_function",
        ];
        let mut outer = symbol_node.walk();
        'outer: for child in symbol_node.children(&mut outer) {
            if fn_wrapper_kinds.contains(&child.kind()) {
                let mut inner = child.walk();
                for grandchild in child.children(&mut inner) {
                    if body_kinds.contains(&grandchild.kind()) {
                        body_start = Some(grandchild.start_byte());
                        break 'outer;
                    }
                }
            }
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

fn fnv1a_hash(data: &[u8]) -> String {
    let mut hash: u64 = 14695981039346656037u64;
    for byte in data {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211u64);
    }
    format!("{:016x}", hash)
}

fn extract_visibility(symbol_node: &Node, source: &str) -> String {
    let mut cursor = symbol_node.walk();
    for child in symbol_node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child
                .utf8_text(source.as_bytes())
                .unwrap_or("pub")
                .to_string();
        }
        // JS/TS: export keyword as first child
        if child.kind() == "export" {
            return "pub".to_string();
        }
    }
    "private".to_string()
}

fn extract_doc(symbol_node: &Node, source: &str) -> Option<String> {
    let start_line = symbol_node.start_position().row;
    if start_line == 0 {
        return None;
    }
    let lines: Vec<&str> = source.lines().collect();
    let mut doc_lines: Vec<String> = Vec::new();
    let mut line_idx = start_line;
    while line_idx > 0 {
        line_idx -= 1;
        let trimmed = lines[line_idx].trim();
        if trimmed.starts_with("///") {
            doc_lines.push(trimmed.trim_start_matches("///").trim().to_string());
        } else if trimmed.starts_with("#[") || trimmed.starts_with("#![") || trimmed.is_empty() {
            // attributes and blank lines between doc and definition are allowed
            continue;
        } else {
            break;
        }
    }
    if doc_lines.is_empty() {
        return None;
    }
    doc_lines.reverse();
    Some(doc_lines.join("\n"))
}

fn extract_file_doc(source: &str, language: &Language) -> Option<String> {
    match language {
        Language::Rust => {
            // Collect consecutive `//!` lines at the top (inner module doc)
            let lines: Vec<&str> = source
                .lines()
                .skip_while(|l| l.trim().is_empty())
                .take_while(|l| l.trim_start().starts_with("//!"))
                .map(|l| {
                    l.trim_start()
                        .trim_start_matches("//!")
                        .trim_start_matches(' ')
                })
                .collect();
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        Language::TypeScript | Language::JavaScript => {
            // First `/** ... */` block before any non-comment code
            let trimmed = source.trim_start();
            if !trimmed.starts_with("/**") {
                return None;
            }
            let end = trimmed.find("*/")?;
            let inner = &trimmed[3..end];
            let text: String = inner
                .lines()
                .map(|l| l.trim_start().trim_start_matches('*').trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Language::Svelte => {
            // `<!-- @component ... -->` convention used by Svelte language tools
            let start = source.find("<!-- @component")?;
            let rest = &source[start + 15..];
            let end = rest.find("-->")?;
            let text = rest[..end].trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
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

    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

    let file_str = path.to_string_lossy().to_string();
    let lang_str = language.as_str().to_string();
    let mut symbols = Vec::new();

    while let Some(m) = matches.next() {
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

            let source_bytes = source.as_bytes();
            let sym_bytes = &source_bytes[symbol_node.start_byte()..symbol_node.end_byte()];
            let content_hash = fnv1a_hash(sym_bytes);
            let visibility = extract_visibility(&symbol_node, source);
            let doc = extract_doc(&symbol_node, source);

            let impl_name = enclosing_impl_name(symbol_node, source);
            let normalized_file = file_str.trim_start_matches("./");
            let stable_id = match &impl_name {
                Some(impl_n) => format!("{}::{}::{}::{}", normalized_file, kind, impl_n, name),
                None => format!("{}::{}::{}", normalized_file, kind, name),
            };

            symbols.push(Symbol {
                name,
                file: file_str.clone(),
                language: lang_str.clone(),
                kind,
                range_start: (symbol_node.start_position().row + 1) as u32,
                range_end: (symbol_node.end_position().row + 1) as u32,
                signature,
                content_hash,
                visibility,
                doc,
                stable_id,
            });
        }
    }

    // Deduplicate: two query patterns can capture the same AST node.
    // Two captures of the same node always share (file, range_start).
    let mut seen = std::collections::HashSet::new();
    symbols.retain(|s| seen.insert((s.file.clone(), s.range_start)));

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

fn enclosing_impl_name(node: Node<'_>, source: &str) -> Option<String> {
    let container_kinds = ["impl_item", "class_declaration", "class"];
    let mut current = node.parent();
    while let Some(n) = current {
        if container_kinds.contains(&n.kind()) {
            // Rust impl_item uses "type" field; TS class uses "name" field
            let name_node = n
                .child_by_field_name("type")
                .or_else(|| n.child_by_field_name("name"));
            return name_node
                .and_then(|tn| tn.utf8_text(source.as_bytes()).ok())
                .map(String::from);
        }
        current = n.parent();
    }
    None
}

fn find_script_raw_texts<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    if node.kind() == "script_element" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "raw_text" {
                out.push(child);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_script_raw_texts(child, out);
    }
}

fn extract_svelte_script_symbols(
    svelte_tree: &tree_sitter::Tree,
    source: &str,
    path: &Path,
) -> Result<(Vec<Symbol>, Vec<RawEdge>)> {
    let mut raw_text_nodes: Vec<Node<'_>> = Vec::new();
    find_script_raw_texts(svelte_tree.root_node(), &mut raw_text_nodes);

    if raw_text_nodes.is_empty() {
        return Ok((vec![], vec![]));
    }

    let ts_language = Language::TypeScript;
    let ts_ts_lang = ts_language.tree_sitter_language();
    let ts_query_src = ts_language.query_source();
    let mut ts_parser = Parser::new();
    ts_parser.set_language(&ts_ts_lang)?;

    let mut all_symbols = Vec::new();
    let mut all_edges = Vec::new();

    for raw_text_node in raw_text_nodes {
        let script_start_row = raw_text_node.start_position().row as u32;
        let raw_text = &source[raw_text_node.start_byte()..raw_text_node.end_byte()];

        let ts_tree = match ts_parser.parse(raw_text, None) {
            Some(t) => t,
            None => continue,
        };

        let mut script_symbols = extract_symbols(
            &ts_tree,
            raw_text,
            path,
            &ts_language,
            &ts_ts_lang,
            ts_query_src,
        )?;
        for sym in &mut script_symbols {
            sym.range_start += script_start_row;
            sym.range_end += script_start_row;
            sym.language = "svelte".to_string();
        }
        all_symbols.extend(script_symbols);

        let script_edges = extract_edges(&ts_tree, raw_text, path, &ts_ts_lang, ts_query_src)?;
        all_edges.extend(script_edges);
    }

    Ok((all_symbols, all_edges))
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

    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    let file_str = path.to_string_lossy().to_string();
    let mut edges = Vec::new();

    while let Some(m) = matches.next() {
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
