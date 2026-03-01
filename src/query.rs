use std::{fs, path::Path};

use anyhow::{anyhow, Result};
use rusqlite::Connection;

pub(crate) fn open_db() -> Result<Connection> {
    let conn = Connection::open(".graphlite/codegraph.db")
        .map_err(|_| anyhow!(".graphlite/codegraph.db not found - run `graphlite init` first"))?;
    Ok(conn)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn resolve_symbol_id(conn: &Connection, arg: &str) -> Result<i64> {
    if let Ok(id) = arg.parse::<i64>() {
        return Ok(id);
    }
    let name = arg.strip_prefix("sym:").unwrap_or(arg);
    let id: i64 = conn
        .query_row(
            "SELECT id FROM nodes WHERE name = ?1 LIMIT 1",
            rusqlite::params![name],
            |r| r.get(0),
        )
        .map_err(|_| anyhow!("symbol '{}' not found in database", name))?;
    Ok(id)
}

pub fn symbols(fts_query: &str, language: Option<&str>) -> Result<()> {
    let conn = open_db()?;

    let mut out = String::from("<symbols>\n");
    let mut count = 0usize;

    if let Some(lang) = language {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.name, n.kind, n.file, n.signature
             FROM fts_symbols f
             JOIN nodes n ON n.id = f.node_id
             WHERE fts_symbols MATCH ?1 AND n.language = ?2
             ORDER BY rank",
        )?;
        let rows = stmt.query_map(rusqlite::params![fts_query, lang], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, name, kind, file, sig) = row?;
            append_symbol_xml(&mut out, id, &name, &kind, &file, sig.as_deref());
            count += 1;
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.name, n.kind, n.file, n.signature
             FROM fts_symbols f
             JOIN nodes n ON n.id = f.node_id
             WHERE fts_symbols MATCH ?1
             ORDER BY rank",
        )?;
        let rows = stmt.query_map(rusqlite::params![fts_query], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        for row in rows {
            let (id, name, kind, file, sig) = row?;
            append_symbol_xml(&mut out, id, &name, &kind, &file, sig.as_deref());
            count += 1;
        }
    }

    out.push_str("</symbols>\n");
    eprintln!("{} match(es)", count);
    print!("{}", out);
    Ok(())
}

fn append_symbol_xml(
    out: &mut String,
    id: i64,
    name: &str,
    kind: &str,
    file: &str,
    sig: Option<&str>,
) {
    out.push_str(&format!(
        "  <symbol id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\"",
        id,
        xml_escape(name),
        xml_escape(kind),
        xml_escape(file)
    ));
    if let Some(s) = sig {
        out.push_str(&format!(" signature=\"{}\"", xml_escape(s)));
    }
    out.push_str("/>\n");
}

struct NodeRow {
    id: i64,
    name: String,
    kind: String,
    file: String,
    range_start: i64,
    range_end: i64,
    signature: Option<String>,
    visibility: String,
    doc: Option<String>,
    fan_in: i64,
    fan_out: i64,
    role: String,
    role_confidence: f64,
}

struct NeighborRow {
    id: i64,
    name: String,
    kind: String,
    file: String,
    range_start: i64,
    range_end: i64,
    signature: Option<String>,
    depth: i64,
    edge_type: Option<String>,
    #[allow(dead_code)]
    source: Option<String>,
    #[allow(dead_code)]
    confidence: Option<f64>,
    visibility: String,
    doc: Option<String>,
    fan_in: i64,
    fan_out: i64,
    role: String,
    role_confidence: f64,
}

fn role_attrs(role: &str, confidence: f64) -> String {
    if confidence < 0.6 {
        format!(
            " role=\"{}\" role_confidence=\"{:.2}\" role_uncertain=\"true\"",
            role, confidence
        )
    } else {
        format!(" role=\"{}\" role_confidence=\"{:.2}\"", role, confidence)
    }
}

struct AnnotationRow {
    intent: Option<String>,
    behavior: Option<String>,
    tags: Option<String>,
    source: String,
    confidence: f64,
    stale: bool,
}

struct EdgeInfo {
    edge_type: String,
    from_id: i64,
    to_id: i64,
    from_name: String,
    to_name: String,
    source: String,
    #[allow(dead_code)]
    confidence: f64,
}

fn fetch_annotation(conn: &Connection, node_id: i64) -> Option<AnnotationRow> {
    conn.query_row(
        "SELECT a.intent, a.behavior, a.tags, a.source, a.confidence,
                a.content_hash_at_annotation != n.content_hash AS stale
         FROM annotations a JOIN nodes n ON n.id = a.node_id
         WHERE a.node_id = ?1",
        rusqlite::params![node_id],
        |r| {
            Ok(AnnotationRow {
                intent: r.get(0)?,
                behavior: r.get(1)?,
                tags: r.get(2)?,
                source: r.get(3)?,
                confidence: r.get(4)?,
                stale: r.get::<_, bool>(5).unwrap_or(false),
            })
        },
    )
    .ok()
}

fn read_snippet(file: &str, line_start: i64, line_end: i64) -> Option<String> {
    let content = fs::read_to_string(Path::new(file)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = ((line_start - 1) as usize).min(lines.len());
    let end = (line_end as usize).min(lines.len());
    Some(lines[start..end].join("\n"))
}

fn read_call_site_snippet(
    file: &str,
    range_start: i64,
    range_end: i64,
    target_name: &str,
) -> Option<String> {
    let content = fs::read_to_string(Path::new(file)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = ((range_start - 1) as usize).min(lines.len());
    let end = (range_end as usize).min(lines.len());
    let body = &lines[start..end];

    let hits: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(target_name))
        .map(|(i, _)| i)
        .collect();

    if hits.is_empty() {
        return None;
    }

    let mut out: Vec<&str> = Vec::new();
    let mut last_end = 0usize;
    for i in hits {
        let ctx_start = i.saturating_sub(1);
        let ctx_end = (i + 2).min(body.len());
        if ctx_start > last_end && !out.is_empty() {
            out.push("...");
        }
        let from = ctx_start.max(last_end);
        out.extend_from_slice(&body[from..ctx_end]);
        last_end = ctx_end;
    }
    Some(out.join("\n"))
}

pub fn graph(
    symbol: &str,
    depth: usize,
    _format: &str,
    show_trust: bool,
    snippets: bool,
) -> Result<()> {
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;

    let focus: NodeRow = conn
        .query_row(
            "SELECT n.id, n.name, n.kind, n.file, n.range_start, n.range_end, n.signature,
                    n.visibility, n.doc,
                    (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                    (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                    n.role, n.role_confidence
             FROM nodes n WHERE n.id = ?1",
            rusqlite::params![root_id],
            |r| {
                Ok(NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(7)?,
                    doc: r.get(8)?,
                    fan_in: r.get(9)?,
                    fan_out: r.get(10)?,
                    role: r.get(11)?,
                    role_confidence: r.get(12)?,
                })
            },
        )
        .map_err(|_| anyhow!("node id {} not found", root_id))?;

    if show_trust {
        let mut edge_stmt = conn.prepare(
            "SELECT e.edge_type, e.from_id, e.to_id,
                    n_from.name, n_to.name, e.source, e.confidence
             FROM edges e
             JOIN nodes n_from ON n_from.id = e.from_id
             JOIN nodes n_to ON n_to.id = e.to_id
             WHERE e.from_id = ?1 OR e.to_id = ?1",
        )?;
        let edges: Vec<EdgeInfo> = edge_stmt
            .query_map(rusqlite::params![root_id], |r| {
                Ok(EdgeInfo {
                    edge_type: r.get(0)?,
                    from_id: r.get(1)?,
                    to_id: r.get(2)?,
                    from_name: r.get(3)?,
                    to_name: r.get(4)?,
                    source: r.get(5)?,
                    confidence: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        print_xml_trust_split(&focus, &edges);
    } else {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE neighborhood(id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT e.to_id, n.depth + 1
                FROM neighborhood n JOIN edges e ON e.from_id = n.id
                WHERE n.depth < ?2
            )
            SELECT DISTINCT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
                   nd.signature, nh.depth,
                   (SELECT edge_type FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
                   (SELECT source FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
                   (SELECT confidence FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
                   nd.visibility, nd.doc,
                   (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
                   (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
                   nd.role, nd.role_confidence
            FROM nodes nd JOIN neighborhood nh ON nd.id = nh.id
            WHERE nd.id != ?1
            ORDER BY nh.depth, nd.name",
        )?;

        let neighbors: Vec<NeighborRow> = stmt
            .query_map(rusqlite::params![root_id, depth as i64], |r| {
                Ok(NeighborRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    depth: r.get(7)?,
                    edge_type: r.get(8)?,
                    source: r.get(9)?,
                    confidence: r.get(10)?,
                    visibility: r.get(11)?,
                    doc: r.get(12)?,
                    fan_in: r.get(13)?,
                    fan_out: r.get(14)?,
                    role: r.get(15)?,
                    role_confidence: r.get(16)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let edge_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE from_id = ?1 OR to_id = ?1",
            rusqlite::params![root_id],
            |r| r.get(0),
        )?;

        print!(
            "{}",
            render_graph_xml(&conn, &focus, &neighbors, edge_count, snippets)
        );
    }

    Ok(())
}

fn render_graph_xml(
    conn: &Connection,
    focus: &NodeRow,
    neighbors: &[NeighborRow],
    edge_count: i64,
    snippets: bool,
) -> String {
    let snippet = read_snippet(&focus.file, focus.range_start, focus.range_end);

    let mut body = String::new();

    body.push_str("  <focus>\n");
    body.push_str(&format!(
        "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"L{}-L{}\" visibility=\"{}\" fan_in=\"{}\" fan_out=\"{}\"{}>\n",
        focus.id,
        xml_escape(&focus.name),
        xml_escape(&focus.kind),
        xml_escape(&focus.file),
        focus.range_start,
        focus.range_end,
        xml_escape(&focus.visibility),
        focus.fan_in,
        focus.fan_out,
        role_attrs(&focus.role, focus.role_confidence),
    ));
    if let Some(sig) = &focus.signature {
        body.push_str(&format!(
            "      <signature>{}</signature>\n",
            xml_escape(sig)
        ));
    }
    if let Some(doc) = &focus.doc {
        if !doc.is_empty() {
            body.push_str(&format!("      <doc>{}</doc>\n", xml_escape(doc)));
        }
    }
    if let Some(ann) = fetch_annotation(conn, focus.id) {
        body.push_str(&format!(
            "      <annotation source=\"{}\" confidence=\"{:.1}\" stale=\"{}\">\n",
            xml_escape(&ann.source),
            ann.confidence,
            ann.stale
        ));
        if let Some(intent) = &ann.intent {
            body.push_str(&format!(
                "        <intent>{}</intent>\n",
                xml_escape(intent)
            ));
        }
        if let Some(behavior) = &ann.behavior {
            body.push_str(&format!(
                "        <behavior>{}</behavior>\n",
                xml_escape(behavior)
            ));
        }
        if let Some(tags) = &ann.tags {
            body.push_str(&format!("        <tags>{}</tags>\n", xml_escape(tags)));
        }
        body.push_str("      </annotation>\n");
    }
    if let Some(snip) = snippet {
        body.push_str(&format!("      <snippet>{}</snippet>\n", xml_escape(&snip)));
    }
    body.push_str("    </node>\n");
    body.push_str("  </focus>\n");

    let max_depth = neighbors.iter().map(|n| n.depth).max().unwrap_or(0);
    for d in 1..=max_depth {
        body.push_str(&format!("  <neighbors depth=\"{}\">\n", d));
        for n in neighbors.iter().filter(|n| n.depth == d) {
            body.push_str(&format!(
                "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"L{}-L{}\" visibility=\"{}\" fan_in=\"{}\" fan_out=\"{}\"{}",
                n.id,
                xml_escape(&n.name),
                xml_escape(&n.kind),
                xml_escape(&n.file),
                n.range_start,
                n.range_end,
                xml_escape(&n.visibility),
                n.fan_in,
                n.fan_out,
                role_attrs(&n.role, n.role_confidence),
            ));
            if let Some(et) = &n.edge_type {
                body.push_str(&format!(" edge_type=\"{}\"", xml_escape(et)));
            }
            if let Some(sig) = &n.signature {
                body.push_str(&format!(" signature=\"{}\"", xml_escape(sig)));
            }
            let has_doc = n.doc.as_ref().is_some_and(|d| !d.is_empty());
            let has_snippet =
                snippets && read_snippet(&n.file, n.range_start, n.range_end).is_some();
            if has_doc || has_snippet {
                body.push_str(">\n");
                if let Some(doc) = &n.doc {
                    if !doc.is_empty() {
                        body.push_str(&format!("      <doc>{}</doc>\n", xml_escape(doc)));
                    }
                }
                if let Some(snip) = read_snippet(&n.file, n.range_start, n.range_end) {
                    body.push_str(&format!("      <snippet>{}</snippet>\n", xml_escape(&snip)));
                }
                body.push_str("    </node>\n");
            } else {
                body.push_str("/>\n");
            }
        }
        body.push_str("  </neighbors>\n");
    }

    body.push_str("</graph>\n");

    let tokens = body.len() / 4;
    let header = format!(
        "<graph root_id=\"{}\" tokens=\"{}\" nodes=\"{}\" edges=\"{}\">\n",
        focus.id,
        tokens,
        1 + neighbors.len(),
        edge_count,
    );

    format!("{}{}", header, body)
}

fn print_xml_trust_split(focus: &NodeRow, edges: &[EdgeInfo]) {
    let snippet = read_snippet(&focus.file, focus.range_start, focus.range_end);

    let trusted: Vec<&EdgeInfo> = edges
        .iter()
        .filter(|e| e.source == "rust-analyzer")
        .collect();
    let syntax: Vec<&EdgeInfo> = edges.iter().filter(|e| e.source == "tree-sitter").collect();

    let mut body = String::new();

    body.push_str("  <focus>\n");
    body.push_str(&format!(
        "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"L{}-L{}\">\n",
        focus.id,
        xml_escape(&focus.name),
        xml_escape(&focus.kind),
        xml_escape(&focus.file),
        focus.range_start,
        focus.range_end,
    ));
    if let Some(sig) = &focus.signature {
        body.push_str(&format!(
            "      <signature>{}</signature>\n",
            xml_escape(sig)
        ));
    }
    if let Some(snip) = snippet {
        body.push_str(&format!("      <snippet>{}</snippet>\n", xml_escape(&snip)));
    }
    body.push_str("    </node>\n");
    body.push_str("  </focus>\n");

    if !trusted.is_empty() {
        body.push_str("  <trusted_edges confidence=\"1.0\">\n");
        for e in &trusted {
            body.push_str(&format!(
                "    <edge type=\"{}\" from_id=\"{}\" to_id=\"{}\" from_name=\"{}\" to_name=\"{}\" source=\"{}\"/>\n",
                xml_escape(&e.edge_type),
                e.from_id,
                e.to_id,
                xml_escape(&e.from_name),
                xml_escape(&e.to_name),
                xml_escape(&e.source),
            ));
        }
        body.push_str("  </trusted_edges>\n");
    }

    if !syntax.is_empty() {
        body.push_str("  <syntax_edges confidence=\"0.8\">\n");
        for e in &syntax {
            body.push_str(&format!(
                "    <edge type=\"{}\" from_id=\"{}\" to_id=\"{}\" from_name=\"{}\" to_name=\"{}\" source=\"{}\"/>\n",
                xml_escape(&e.edge_type),
                e.from_id,
                e.to_id,
                xml_escape(&e.from_name),
                xml_escape(&e.to_name),
                xml_escape(&e.source),
            ));
        }
        body.push_str("  </syntax_edges>\n");
    }

    body.push_str("</graph>\n");

    let tokens = body.len() / 4;
    let header = format!(
        "<graph root_id=\"{}\" tokens=\"{}\" trusted_edges=\"{}\" syntax_edges=\"{}\">\n",
        focus.id,
        tokens,
        trusted.len(),
        syntax.len(),
    );

    print!("{}{}", header, body);
}

pub fn blast_radius(symbol: &str, depth: usize, snippets: bool) -> Result<()> {
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;

    let depth_limit = if depth == 0 { 50_i64 } else { depth as i64 };

    let mut stmt = conn.prepare(
        "WITH RECURSIVE dependents(id, depth) AS (
            SELECT ?1, 0
            UNION ALL
            SELECT e.from_id, d.depth + 1
            FROM dependents d JOIN edges e ON e.to_id = d.id
            WHERE d.depth < ?2
        )
        SELECT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
               nd.signature, MIN(nh.depth),
               nd.visibility, nd.doc,
               (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
               (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
               nd.role, nd.role_confidence
        FROM nodes nd JOIN dependents nh ON nd.id = nh.id
        WHERE nd.id != ?1
        GROUP BY nd.id
        ORDER BY MIN(nh.depth), nd.name",
    )?;

    let rows: Vec<(NodeRow, i64)> = stmt
        .query_map(rusqlite::params![root_id, depth_limit], |r| {
            Ok((
                NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(8)?,
                    doc: r.get(9)?,
                    fan_in: r.get(10)?,
                    fan_out: r.get(11)?,
                    role: r.get(12)?,
                    role_confidence: r.get(13)?,
                },
                r.get::<_, i64>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let focus: NodeRow = conn
        .query_row(
            "SELECT n.id, n.name, n.kind, n.file, n.range_start, n.range_end, n.signature,
                    n.visibility, n.doc,
                    (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                    (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                    n.role, n.role_confidence
             FROM nodes n WHERE n.id = ?1",
            rusqlite::params![root_id],
            |r| {
                Ok(NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(7)?,
                    doc: r.get(8)?,
                    fan_in: r.get(9)?,
                    fan_out: r.get(10)?,
                    role: r.get(11)?,
                    role_confidence: r.get(12)?,
                })
            },
        )
        .map_err(|_| anyhow!("node id {} not found", root_id))?;

    print!(
        "{}",
        render_blast_radius_xml(&conn, &focus, &rows, snippets)
    );
    Ok(())
}

pub fn context(symbol: &str, depth: usize, blast_depth: usize, snippets: bool) -> Result<()> {
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;

    // --- graph neighborhood ---
    let focus: NodeRow = conn
        .query_row(
            "SELECT n.id, n.name, n.kind, n.file, n.range_start, n.range_end, n.signature,
                    n.visibility, n.doc,
                    (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                    (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                    n.role, n.role_confidence
             FROM nodes n WHERE n.id = ?1",
            rusqlite::params![root_id],
            |r| {
                Ok(NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(7)?,
                    doc: r.get(8)?,
                    fan_in: r.get(9)?,
                    fan_out: r.get(10)?,
                    role: r.get(11)?,
                    role_confidence: r.get(12)?,
                })
            },
        )
        .map_err(|_| anyhow!("node id {} not found", root_id))?;

    let mut neighbor_stmt = conn.prepare(
        "WITH RECURSIVE neighborhood(id, depth) AS (
            SELECT ?1, 0
            UNION ALL
            SELECT e.to_id, n.depth + 1
            FROM neighborhood n JOIN edges e ON e.from_id = n.id
            WHERE n.depth < ?2
        )
        SELECT DISTINCT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
               nd.signature, nh.depth,
               (SELECT edge_type FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
               (SELECT source FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
               (SELECT confidence FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
               nd.visibility, nd.doc,
               (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
               (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
               nd.role, nd.role_confidence
        FROM nodes nd JOIN neighborhood nh ON nd.id = nh.id
        WHERE nd.id != ?1
        ORDER BY nh.depth, nd.name",
    )?;
    let neighbors: Vec<NeighborRow> = neighbor_stmt
        .query_map(rusqlite::params![root_id, depth as i64], |r| {
            Ok(NeighborRow {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                file: r.get(3)?,
                range_start: r.get(4)?,
                range_end: r.get(5)?,
                signature: r.get(6)?,
                depth: r.get(7)?,
                edge_type: r.get(8)?,
                source: r.get(9)?,
                confidence: r.get(10)?,
                visibility: r.get(11)?,
                doc: r.get(12)?,
                fan_in: r.get(13)?,
                fan_out: r.get(14)?,
                role: r.get(15)?,
                role_confidence: r.get(16)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let edge_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE from_id = ?1 OR to_id = ?1",
        rusqlite::params![root_id],
        |r| r.get(0),
    )?;
    let graph_xml = render_graph_xml(&conn, &focus, &neighbors, edge_count, snippets);

    // --- blast radius (callers) ---
    let blast_limit = if blast_depth == 0 {
        50_i64
    } else {
        blast_depth as i64
    };
    let mut dep_stmt = conn.prepare(
        "WITH RECURSIVE dependents(id, depth) AS (
            SELECT ?1, 0
            UNION ALL
            SELECT e.from_id, d.depth + 1
            FROM dependents d JOIN edges e ON e.to_id = d.id
            WHERE d.depth < ?2
        )
        SELECT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
               nd.signature, MIN(nh.depth),
               nd.visibility, nd.doc,
               (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
               (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
               nd.role, nd.role_confidence
        FROM nodes nd JOIN dependents nh ON nd.id = nh.id
        WHERE nd.id != ?1
        GROUP BY nd.id
        ORDER BY MIN(nh.depth), nd.name",
    )?;
    let dep_rows: Vec<(NodeRow, i64)> = dep_stmt
        .query_map(rusqlite::params![root_id, blast_limit], |r| {
            Ok((
                NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(8)?,
                    doc: r.get(9)?,
                    fan_in: r.get(10)?,
                    fan_out: r.get(11)?,
                    role: r.get(12)?,
                    role_confidence: r.get(13)?,
                },
                r.get::<_, i64>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let blast_xml = render_blast_radius_xml(&conn, &focus, &dep_rows, snippets);

    // --- unified context document ---
    let total_tokens = (graph_xml.len() + blast_xml.len()) / 4;
    println!(
        "<context symbol=\"{}\" root_id=\"{}\" total_tokens=\"{}\">\n{}{}</context>",
        xml_escape(&focus.name),
        root_id,
        total_tokens,
        graph_xml,
        blast_xml,
    );

    Ok(())
}

fn render_blast_radius_xml(
    conn: &Connection,
    focus: &NodeRow,
    dependents: &[(NodeRow, i64)],
    snippets: bool,
) -> String {
    let snippet = read_snippet(&focus.file, focus.range_start, focus.range_end);

    let mut body = String::new();

    body.push_str("  <focus>\n");
    body.push_str(&format!(
        "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"L{}-L{}\" visibility=\"{}\" fan_in=\"{}\" fan_out=\"{}\"{}>\n",
        focus.id,
        xml_escape(&focus.name),
        xml_escape(&focus.kind),
        xml_escape(&focus.file),
        focus.range_start,
        focus.range_end,
        xml_escape(&focus.visibility),
        focus.fan_in,
        focus.fan_out,
        role_attrs(&focus.role, focus.role_confidence),
    ));
    if let Some(sig) = &focus.signature {
        body.push_str(&format!(
            "      <signature>{}</signature>\n",
            xml_escape(sig)
        ));
    }
    if let Some(doc) = &focus.doc {
        if !doc.is_empty() {
            body.push_str(&format!("      <doc>{}</doc>\n", xml_escape(doc)));
        }
    }
    if let Some(ann) = fetch_annotation(conn, focus.id) {
        body.push_str(&format!(
            "      <annotation source=\"{}\" confidence=\"{:.1}\" stale=\"{}\">\n",
            xml_escape(&ann.source),
            ann.confidence,
            ann.stale
        ));
        if let Some(intent) = &ann.intent {
            body.push_str(&format!(
                "        <intent>{}</intent>\n",
                xml_escape(intent)
            ));
        }
        if let Some(behavior) = &ann.behavior {
            body.push_str(&format!(
                "        <behavior>{}</behavior>\n",
                xml_escape(behavior)
            ));
        }
        if let Some(tags) = &ann.tags {
            body.push_str(&format!("        <tags>{}</tags>\n", xml_escape(tags)));
        }
        body.push_str("      </annotation>\n");
    }
    if let Some(snip) = snippet {
        body.push_str(&format!("      <snippet>{}</snippet>\n", xml_escape(&snip)));
    }
    body.push_str("    </node>\n");
    body.push_str("  </focus>\n");

    let max_depth = dependents.iter().map(|(_, d)| *d).max().unwrap_or(0);
    for d in 1..=max_depth {
        body.push_str(&format!("  <dependents depth=\"{}\">\n", d));
        for (n, _) in dependents.iter().filter(|(_, dep)| *dep == d) {
            body.push_str(&format!(
                "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"L{}-L{}\" visibility=\"{}\" fan_in=\"{}\" fan_out=\"{}\"{}",
                n.id,
                xml_escape(&n.name),
                xml_escape(&n.kind),
                xml_escape(&n.file),
                n.range_start,
                n.range_end,
                xml_escape(&n.visibility),
                n.fan_in,
                n.fan_out,
                role_attrs(&n.role, n.role_confidence),
            ));
            if let Some(sig) = &n.signature {
                body.push_str(&format!(" signature=\"{}\"", xml_escape(sig)));
            }
            let has_doc = n.doc.as_ref().is_some_and(|d| !d.is_empty());
            let has_snippet = snippets
                && read_call_site_snippet(&n.file, n.range_start, n.range_end, &focus.name)
                    .is_some();
            if has_doc || has_snippet {
                body.push_str(">\n");
                if let Some(doc) = &n.doc {
                    if !doc.is_empty() {
                        body.push_str(&format!("      <doc>{}</doc>\n", xml_escape(doc)));
                    }
                }
                if let Some(snip) =
                    read_call_site_snippet(&n.file, n.range_start, n.range_end, &focus.name)
                {
                    body.push_str(&format!("      <snippet>{}</snippet>\n", xml_escape(&snip)));
                }
                body.push_str("    </node>\n");
            } else {
                body.push_str("/>\n");
            }
        }
        body.push_str("  </dependents>\n");
    }

    body.push_str("</blast_radius>\n");

    let tokens = body.len() / 4;
    let header = format!(
        "<blast_radius root_id=\"{}\" root_name=\"{}\" tokens=\"{}\" dependent_count=\"{}\">\n",
        focus.id,
        xml_escape(&focus.name),
        tokens,
        dependents.len(),
    );

    format!("{}{}", header, body)
}

pub fn map(
    include_private: bool,
    top: usize,
    with_docs: bool,
    with_file_docs: bool,
    all_edges: bool,
) -> Result<()> {
    let conn = open_db()?;

    struct Row {
        file: String,
        name: String,
        kind: String,
        visibility: String,
        signature: Option<String>,
        hotspot_fan_in: i64,
        fan_in: i64,
        fan_out: i64,
        doc: Option<String>,
        role: String,
        role_confidence: f64,
    }

    let hotspot_subquery = if all_edges {
        "SELECT COUNT(*) FROM edges WHERE to_id = n.id"
    } else {
        "SELECT COUNT(*) FROM edges WHERE to_id = n.id AND source = 'rust-analyzer'"
    };

    let visibility_filter = if include_private {
        ""
    } else {
        "WHERE n.visibility != 'private'"
    };

    let sql = format!(
        "SELECT n.file, n.name, n.kind, n.visibility, n.signature,
                ({hotspot_subquery}) AS hotspot_fan_in,
                (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                n.doc, n.role, n.role_confidence
         FROM nodes n
         {visibility_filter}
         ORDER BY n.file, hotspot_fan_in DESC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok(Row {
                file: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                visibility: r.get(3)?,
                signature: r.get(4)?,
                hotspot_fan_in: r.get(5)?,
                fan_in: r.get(6)?,
                fan_out: r.get(7)?,
                doc: r.get(8)?,
                role: r.get(9)?,
                role_confidence: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // File-level docs: path -> doc (only loaded when --with-file-docs)
    let file_doc_map: std::collections::HashMap<String, String> = if with_file_docs {
        let mut fstmt =
            conn.prepare("SELECT path, doc FROM files WHERE doc IS NOT NULL AND doc != ''")?;
        let pairs: Vec<(String, String)> = fstmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        pairs.into_iter().collect()
    } else {
        std::collections::HashMap::new()
    };

    // Group by file (BTreeMap keeps files in alphabetical order)
    let mut by_file: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        by_file.entry(row.file.clone()).or_default().push(i);
    }

    // Hotspots: top N across all files by hotspot_fan_in (trusted by default)
    let mut ranked: Vec<usize> = (0..rows.len()).collect();
    ranked.sort_by(|&a, &b| rows[b].hotspot_fan_in.cmp(&rows[a].hotspot_fan_in));
    ranked.truncate(top);
    // Drop zero-count entries — no trusted edges means no signal
    ranked.retain(|&i| rows[i].hotspot_fan_in > 0);

    let mut out = String::new();

    let hotspot_source = if all_edges { "all" } else { "trusted" };
    out.push_str(&format!(
        "  <hotspots top=\"{}\" fan_in_source=\"{}\">\n",
        ranked.len(),
        hotspot_source,
    ));
    for &i in &ranked {
        let s = &rows[i];
        out.push_str(&format!(
            "    <symbol name=\"{}\" kind=\"{}\" file=\"{}\" fan_in=\"{}\" fan_out=\"{}\"/>\n",
            xml_escape(&s.name),
            xml_escape(&s.kind),
            xml_escape(&s.file),
            s.hotspot_fan_in,
            s.fan_out,
        ));
    }
    out.push_str("  </hotspots>\n");

    for (file, indices) in &by_file {
        let file_doc = if with_file_docs {
            file_doc_map.get(file.as_str())
        } else {
            None
        };
        if let Some(fd) = file_doc {
            out.push_str(&format!(
                "  <file path=\"{}\" symbols=\"{}\">\n    <doc>{}</doc>\n",
                xml_escape(file),
                indices.len(),
                xml_escape(fd),
            ));
        } else {
            out.push_str(&format!(
                "  <file path=\"{}\" symbols=\"{}\">\n",
                xml_escape(file),
                indices.len(),
            ));
        }
        for &i in indices {
            let s = &rows[i];
            out.push_str(&format!(
                "    <symbol name=\"{}\" kind=\"{}\" visibility=\"{}\" fan_in=\"{}\" fan_out=\"{}\"{}",
                xml_escape(&s.name),
                xml_escape(&s.kind),
                xml_escape(&s.visibility),
                s.fan_in,
                s.fan_out,
                role_attrs(&s.role, s.role_confidence),
            ));
            if let Some(sig) = &s.signature {
                out.push_str(&format!(" signature=\"{}\"", xml_escape(sig)));
            }
            let sym_doc = if with_docs {
                s.doc.as_deref().filter(|d| !d.is_empty())
            } else {
                None
            };
            if let Some(doc) = sym_doc {
                out.push_str(">\n");
                out.push_str(&format!("      <doc>{}</doc>\n", xml_escape(doc)));
                out.push_str("    </symbol>\n");
            } else {
                out.push_str("/>\n");
            }
        }
        out.push_str("  </file>\n");
    }

    out.push_str("</map>\n");

    let tokens = out.len() / 4;
    let total_symbols: usize = by_file.values().map(|v| v.len()).sum();
    print!(
        "<map files=\"{}\" symbols=\"{}\" tokens=\"{}\">\n{}",
        by_file.len(),
        total_symbols,
        tokens,
        out,
    );

    Ok(())
}
