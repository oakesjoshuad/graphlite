use anyhow::{anyhow, Result};
use rusqlite::Connection;
use tracing::info;

use crate::{
    config,
    ipc::{self, WatchMsg},
    query::{open_db, resolve_symbol_id},
    xml as vxml,
};

/// Write or update an annotation using an already-open connection.
/// Called directly by the watcher server (which owns the connection).
pub fn annotate_with_conn(
    conn: &Connection,
    symbol: &str,
    intent: Option<&str>,
    behavior: Option<&str>,
    tags: Option<&str>,
    source: &str,
    confidence: f64,
) -> Result<()> {
    if intent.is_none() && behavior.is_none() && tags.is_none() {
        return Err(anyhow!(
            "at least one of --intent, --behavior, --tags is required"
        ));
    }

    let node_id = resolve_symbol_id(conn, symbol)?;

    let content_hash: String = conn.query_row(
        "SELECT content_hash FROM nodes WHERE id = ?1",
        rusqlite::params![node_id],
        |r| r.get(0),
    )?;

    conn.execute(
        "INSERT INTO annotations (node_id, intent, behavior, tags, source, confidence, content_hash_at_annotation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(node_id) DO UPDATE SET
             intent = COALESCE(?2, intent),
             behavior = COALESCE(?3, behavior),
             tags = COALESCE(?4, tags),
             source = ?5,
             confidence = ?6,
             content_hash_at_annotation = ?7",
        rusqlite::params![node_id, intent, behavior, tags, source, confidence, content_hash],
    )?;

    let node_name: String = conn.query_row(
        "SELECT name FROM nodes WHERE id = ?1",
        rusqlite::params![node_id],
        |r| r.get(0),
    )?;
    info!(node = %node_name, node_id, "annotated");
    Ok(())
}

/// Public entrypoint for `graphlite annotate`.
/// Tries the watcher socket first; falls back to a direct DB write.
pub fn annotate(
    symbol: &str,
    intent: Option<&str>,
    behavior: Option<&str>,
    tags: Option<&str>,
    source: &str,
    confidence: f64,
) -> Result<()> {
    if intent.is_none() && behavior.is_none() && tags.is_none() {
        return Err(anyhow!(
            "at least one of --intent, --behavior, --tags is required"
        ));
    }

    // Try the watcher socket first.
    let root = config::find_root(".").unwrap_or_else(|_| ".".to_string());
    if ipc::is_watcher_running(&root) {
        let resp = ipc::send_msg(
            &root,
            &WatchMsg::Annotate {
                symbol: symbol.to_string(),
                intent: intent.map(str::to_string),
                behavior: behavior.map(str::to_string),
                tags: tags.map(str::to_string),
                source: source.to_string(),
                confidence,
            },
        )?;
        if resp.ok {
            return Ok(());
        }
        return Err(anyhow!(
            "watcher annotate failed: {}",
            resp.error.unwrap_or_default()
        ));
    }

    // Fallback: direct write.
    let conn = open_db()?;
    annotate_with_conn(&conn, symbol, intent, behavior, tags, source, confidence)
}

/// List annotations, optionally filtered to stale ones only.
/// Stale = content_hash_at_annotation differs from current nodes.content_hash.
pub fn list_annotations(stale_only: bool) -> Result<()> {
    let conn = open_db()?;

    let sql = if stale_only {
        "SELECT a.id, n.name, n.kind, n.file, a.intent, a.behavior, a.tags,
                a.source, a.confidence,
                (a.content_hash_at_annotation != n.content_hash) AS stale
         FROM annotations a JOIN nodes n ON n.id = a.node_id
         WHERE stale = 1
         ORDER BY n.name"
    } else {
        "SELECT a.id, n.name, n.kind, n.file, a.intent, a.behavior, a.tags,
                a.source, a.confidence,
                (a.content_hash_at_annotation != n.content_hash) AS stale
         FROM annotations a JOIN nodes n ON n.id = a.node_id
         ORDER BY n.name"
    };

    let mut stmt = conn.prepare(sql)?;
    let mut count = 0usize;
    let mut w = vxml::new_stream_writer();
    vxml::open(&mut w, "annotations")?;

    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, f64>(8)?,
            r.get::<_, bool>(9)?,
        ))
    })?;

    for row in rows {
        let (id, name, kind, file, intent, behavior, tags, source, confidence, stale) = row?;
        let id_s = id.to_string();
        let conf_s = format!("{:.1}", confidence);
        let stale_s = stale.to_string();
        vxml::open_attrs(
            &mut w,
            "annotation",
            &[
                ("id", &id_s),
                ("name", &name),
                ("kind", &kind),
                ("file", &file),
                ("source", &source),
                ("confidence", &conf_s),
                ("stale", &stale_s),
            ],
        )?;
        if let Some(i) = &intent {
            vxml::text_tag(&mut w, "intent", i)?;
        }
        if let Some(b) = &behavior {
            vxml::text_tag(&mut w, "behavior", b)?;
        }
        if let Some(t) = &tags {
            vxml::text_tag(&mut w, "tags", t)?;
        }
        vxml::close(&mut w, "annotation")?;
        count += 1;
    }

    vxml::close(&mut w, "annotations")?;
    vxml::finish_stream(w)?;
    info!(count, "annotations listed");
    Ok(())
}
