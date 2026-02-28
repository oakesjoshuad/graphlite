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

pub fn symbols(fts_query: &str) -> Result<()> {
    let conn = open_db()?;

    let mut stmt = conn.prepare(
        "SELECT n.id, n.name, n.kind, n.file, n.signature
         FROM fts_symbols f
         JOIN nodes n ON n.id = f.node_id
         WHERE fts_symbols MATCH ?1
         ORDER BY rank",
    )?;

    let mut out = String::from("<symbols>\n");
    let mut count = 0usize;

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
        out.push_str(&format!(
            "  <symbol id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\"",
            id,
            xml_escape(&name),
            xml_escape(&kind),
            xml_escape(&file)
        ));
        if let Some(s) = sig {
            out.push_str(&format!(" signature=\"{}\"", xml_escape(&s)));
        }
        out.push_str("/>\n");
        count += 1;
    }

    out.push_str("</symbols>\n");
    eprintln!("{} match(es)", count);
    print!("{}", out);
    Ok(())
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
}

fn read_snippet(file: &str, range_start: i64, range_end: i64) -> Option<String> {
    let content = fs::read(Path::new(file)).ok()?;
    let start = (range_start as usize).min(content.len());
    let end = (range_end as usize).min(content.len());
    std::str::from_utf8(&content[start..end])
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn graph(symbol: &str, depth: usize, format: &str) -> Result<()> {
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;

    // Fetch the focus node
    let focus: NodeRow = conn.query_row(
        "SELECT id, name, kind, file, range_start, range_end, signature FROM nodes WHERE id = ?1",
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
    ).map_err(|_| anyhow!("node id {} not found", root_id))?;

    // Fetch neighbors via recursive CTE
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
               (SELECT edge_type FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1)
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
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Count edges for the focus node
    let edge_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE from_id = ?1 OR to_id = ?1",
        rusqlite::params![root_id],
        |r| r.get(0),
    )?;

    if format == "xml" || format == "default" {
        print_xml(&focus, &neighbors, depth, edge_count);
    } else {
        print_xml(&focus, &neighbors, depth, edge_count);
    }

    Ok(())
}

fn print_xml(focus: &NodeRow, neighbors: &[NeighborRow], depth: usize, edge_count: i64) {
    let snippet = read_snippet(&focus.file, focus.range_start, focus.range_end);

    let mut out = format!(
        "<graph root_id=\"{}\" nodes=\"{}\" edges=\"{}\">\n",
        focus.id,
        1 + neighbors.len(),
        edge_count,
    );

    out.push_str("  <focus>\n");
    out.push_str(&format!(
        "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"{}-{}\">\n",
        focus.id,
        xml_escape(&focus.name),
        xml_escape(&focus.kind),
        xml_escape(&focus.file),
        focus.range_start,
        focus.range_end,
    ));
    if let Some(sig) = &focus.signature {
        out.push_str(&format!(
            "      <signature>{}</signature>\n",
            xml_escape(sig)
        ));
    }
    if let Some(snip) = snippet {
        out.push_str(&format!(
            "      <snippet>{}</snippet>\n",
            xml_escape(&snip)
        ));
    }
    out.push_str("    </node>\n");
    out.push_str("  </focus>\n");

    // Group neighbors by depth
    let max_depth = neighbors.iter().map(|n| n.depth).max().unwrap_or(0);
    for d in 1..=max_depth {
        out.push_str(&format!("  <neighbors depth=\"{}\">\n", d));
        for n in neighbors.iter().filter(|n| n.depth == d) {
            out.push_str(&format!(
                "    <node id=\"{}\" name=\"{}\" kind=\"{}\" file=\"{}\" range=\"{}-{}\"",
                n.id,
                xml_escape(&n.name),
                xml_escape(&n.kind),
                xml_escape(&n.file),
                n.range_start,
                n.range_end,
            ));
            if let Some(et) = &n.edge_type {
                out.push_str(&format!(" edge_type=\"{}\"", xml_escape(et)));
            }
            if let Some(sig) = &n.signature {
                out.push_str(&format!(" signature=\"{}\"", xml_escape(sig)));
            }
            out.push_str("/>\n");
        }
        out.push_str("  </neighbors>\n");
    }

    out.push_str("</graph>\n");
    print!("{}", out);

    let _ = depth; // suppress unused warning
}
