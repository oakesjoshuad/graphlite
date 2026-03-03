use anyhow::{bail, Result};

use crate::query::{open_db, resolve_symbol_id};

pub struct SetParams<'a> {
    pub symbol: &'a str,
    pub role: &'a str,
    pub reason: Option<&'a str>,
}

/// Override the role for a symbol by stable_id. Survives re-index.
pub fn set(params: &SetParams) -> Result<()> {
    let conn = open_db()?;
    let node_id = resolve_symbol_id(&conn, params.symbol)?;
    let stable_id: String = conn.query_row(
        "SELECT stable_id FROM nodes WHERE id = ?1",
        [node_id],
        |r| r.get(0),
    )?;
    if stable_id.is_empty() {
        bail!("Symbol has no stable_id — cannot create a persistent override");
    }
    conn.execute(
        "INSERT INTO role_overrides (stable_id, role, reason)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(stable_id) DO UPDATE SET role = excluded.role, reason = excluded.reason",
        rusqlite::params![stable_id, params.role, params.reason],
    )?;
    // Apply immediately so violations/map queries see the new role without a re-discover.
    conn.execute(
        "UPDATE nodes SET role = ?2, role_confidence = 1.0 WHERE stable_id = ?1",
        rusqlite::params![stable_id, params.role],
    )?;
    println!("Override set: {} → {}", stable_id, params.role);
    Ok(())
}

/// Remove the role override for a symbol.
pub fn clear(symbol: &str) -> Result<()> {
    let conn = open_db()?;
    let node_id = resolve_symbol_id(&conn, symbol)?;
    let stable_id: String = conn.query_row(
        "SELECT stable_id FROM nodes WHERE id = ?1",
        [node_id],
        |r| r.get(0),
    )?;
    let n = conn.execute(
        "DELETE FROM role_overrides WHERE stable_id = ?1",
        rusqlite::params![stable_id],
    )?;
    if n == 0 {
        println!("No override found for {}", stable_id);
    } else {
        println!("Override cleared: {}", stable_id);
    }
    Ok(())
}

/// List all active role overrides.
pub fn list() -> Result<()> {
    let conn = open_db()?;
    let mut stmt = conn.prepare(
        "SELECT ro.stable_id, ro.role, ro.reason, ro.created_at,
                n.role AS current_role
         FROM role_overrides ro
         LEFT JOIN nodes n ON n.stable_id = ro.stable_id
         ORDER BY ro.stable_id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, Option<String>, String, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    if rows.is_empty() {
        println!("No role overrides set.");
        return Ok(());
    }

    println!(
        "{:<60} {:<15} {:<15} reason",
        "stable_id", "override", "current"
    );
    println!("{}", "-".repeat(110));
    for (stable_id, role, reason, _created_at, current_role) in &rows {
        println!(
            "{:<60} {:<15} {:<15} {}",
            stable_id,
            role,
            current_role.as_deref().unwrap_or("(not found)"),
            reason.as_deref().unwrap_or(""),
        );
    }
    Ok(())
}
