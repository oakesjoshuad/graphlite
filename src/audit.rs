use std::process::Command;

use anyhow::Result;
use tracing::info;
use rusqlite::Connection;

use crate::schema::open_or_init_db;

pub fn run(root: &str) -> Result<()> {
    let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
    std::fs::create_dir_all(&graphlite_dir)?;
    let db_path = format!("{}/codegraph.db", graphlite_dir);
    let conn = open_or_init_db(&db_path)?;
    let total = enrich(root, &conn)?;
    println!("audit: total={}", total);
    Ok(())
}

/// Run cargo-audit and refresh `crate_advisories`. Returns inserted row count.
pub fn enrich(root: &str, conn: &Connection) -> Result<usize> {
    let output = match Command::new("cargo")
        .args(["audit", "--json"])
        .current_dir(root)
        .output()
    {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "cargo audit is not installed. Install with: cargo install cargo-audit"
            );
            return Ok(0);
        }
        Err(e) => return Err(e.into()),
    };

    // cargo-audit often exits non-zero when vulnerabilities are found, so parse
    // stdout regardless of status.
    if output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no such command: `audit`") {
            println!(
                "cargo audit is not installed. Install with: cargo install cargo-audit"
            );
            return Ok(0);
        }
        if stderr.contains("advisory-db")
            && (stderr.contains("read-only path") || stderr.contains("lock file"))
        {
            println!(
                "cargo audit advisory DB is not writable in this environment; rerun where ~/.cargo is writable."
            );
            return Ok(0);
        }
        anyhow::bail!("cargo audit returned no JSON output: {}", stderr.trim());
    }

    let doc: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let advisories = collect_advisories(&doc);

    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM crate_advisories", [])?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO crate_advisories
             (package_name, version, advisory_id, title, cvss, severity, category, kind, date)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;

        for a in &advisories {
            stmt.execute(rusqlite::params![
                &a.package_name,
                &a.version,
                &a.advisory_id,
                &a.title,
                a.cvss.as_deref(),
                &a.severity,
                a.category.as_deref(),
                &a.kind,
                a.date.as_deref(),
            ])?;
        }
    }
    tx.commit()?;

    let total = advisories.len();
    let vulnerabilities = advisories.iter().filter(|a| a.kind == "vulnerability").count();
    let unmaintained = advisories.iter().filter(|a| a.kind == "unmaintained").count();
    let yanked = advisories.iter().filter(|a| a.kind == "yanked").count();
    info!(total, vulnerabilities, unmaintained, yanked, "audit complete");

    Ok(total)
}

struct AdvisoryRow {
    package_name: String,
    version: String,
    advisory_id: String,
    title: String,
    cvss: Option<String>,
    severity: String,
    category: Option<String>,
    kind: String,
    date: Option<String>,
}

fn collect_advisories(doc: &serde_json::Value) -> Vec<AdvisoryRow> {
    let mut out = Vec::new();

    if let Some(vulns) = doc["vulnerabilities"]["list"].as_array() {
        for v in vulns {
            if let Some(row) = parse_entry(v, "vulnerability") {
                out.push(row);
            }
        }
    }

    for kind in ["unmaintained", "yanked", "unsound"] {
        if let Some(items) = doc["warnings"][kind].as_array() {
            for v in items {
                if let Some(row) = parse_entry(v, kind) {
                    out.push(row);
                }
            }
        }
    }

    out
}

fn parse_entry(v: &serde_json::Value, kind: &str) -> Option<AdvisoryRow> {
    let package_name = v["package"]["name"].as_str()?.to_string();
    let version = v["package"]["version"].as_str()?.to_string();
    let advisory_id = v["advisory"]["id"].as_str()?.to_string();
    let title = v["advisory"]["title"].as_str()?.to_string();
    let cvss = v["advisory"]["cvss"].as_str().map(str::to_string);
    let category = v["advisory"]["categories"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|c| c.as_str())
        .map(str::to_string);
    let date = v["advisory"]["date"].as_str().map(str::to_string);

    Some(AdvisoryRow {
        package_name,
        version,
        advisory_id,
        title,
        severity: derive_severity(cvss.as_deref()),
        cvss,
        category,
        kind: kind.to_string(),
        date,
    })
}

fn derive_severity(cvss: Option<&str>) -> String {
    let Some(v) = cvss else {
        return "informational".to_string();
    };

    // Heuristic severity mapping from CVSS vector when numeric base score is not present.
    if v.contains("AV:N") && v.contains("AC:L") && v.contains("PR:N") {
        "high".to_string()
    } else if v.contains("AV:N") {
        "medium".to_string()
    } else if v.contains("AV:L") {
        "low".to_string()
    } else {
        "informational".to_string()
    }
}
