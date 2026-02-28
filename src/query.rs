use std::{fs, path::Path};

use anyhow::{anyhow, Result};
use rusqlite::Connection;

fn open_db() -> Result<Connection> {
    let conn = Connection::open("codegraph.db")
        .map_err(|_| anyhow!("codegraph.db not found - run `graphlite discover` first"))?;
    Ok(conn)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn resolve_symbol_id(conn: &Connection, arg: &str) -> Result<i64> {
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

fn read_snippet(file: &str, line_start: i64, line_end: i64) -> Option<String> {
    let content = fs::read_to_string(Path::new(file)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = ((line_start - 1) as usize).min(lines.len());
    let end = (line_end as usize).min(lines.len());
    Some(lines[start..end].join("\n"))
}

pub fn graph(symbol: &str, depth: usize, _format: &str, show_trust: bool) -> Result<()> {
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;

    let focus: NodeRow = conn
        .query_row(
            "SELECT id, name, kind, file, range_start, range_end, signature
             FROM nodes WHERE id = ?1",
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
                   (SELECT confidence FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1)
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let edge_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE from_id = ?1 OR to_id = ?1",
            rusqlite::params![root_id],
            |r| r.get(0),
        )?;

        print_xml(&focus, &neighbors, edge_count);
    }

    Ok(())
}

fn print_xml(focus: &NodeRow, neighbors: &[NeighborRow], edge_count: i64) {
    let snippet = read_snippet(&focus.file, focus.range_start, focus.range_end);

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

    let max_depth = neighbors.iter().map(|n| n.depth).max().unwrap_or(0);
    for d in 1..=max_depth {
        body.push_str(&format!("  <neighbors depth=\"{}\">\n", d));
        for n in neighbors.iter().filter(|n| n.depth == d) {
            body.push_str(&format!(
                "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"L{}-L{}\"",
                n.id,
                xml_escape(&n.name),
                xml_escape(&n.kind),
                xml_escape(&n.file),
                n.range_start,
                n.range_end,
            ));
            if let Some(et) = &n.edge_type {
                body.push_str(&format!(" edge_type=\"{}\"", xml_escape(et)));
            }
            if let Some(sig) = &n.signature {
                body.push_str(&format!(" signature=\"{}\"", xml_escape(sig)));
            }
            body.push_str("/>\n");
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

    print!("{}{}", header, body);
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
