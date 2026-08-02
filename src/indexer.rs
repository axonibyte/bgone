//! Builds the cache by evaluating every port in the tree.
//!
//! This used to be a regex sweep over Makefile text. It read `Makefile*`,
//! followed `MASTERDIR` and quoted `.include`s by hand, expanded variables with
//! a five-round substitution, and derived a dependency's target by scanning for
//! anything shaped like `word/word`. It could not evaluate `.if` or `.for`, knew
//! nothing of `Mk/Uses`, and wrote edges it then had to delete again.
//!
//! Now [`crate::resolve`] asks make, and this module's job is only to decide
//! *which* ports to ask about, and to turn the answers into rows. Two passes,
//! because an edge cannot be written before the port it points at has an id:
//!
//! 1. Enumerate `category/port` directories and insert a row for each, so every
//!    possible dependency target has an id.
//! 2. Evaluate each port and write its options, flavours and edges.
//!
//! The evaluation pass is the expensive one — one `make` per port — so it is
//! parallel and keyed on Makefile mtime: re-indexing after a tree update only
//! re-evaluates what changed.

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::resolve::{self, MakeEnv, PortFacts, REASON_NO_SUCH_PORT};

pub struct IndexerStats {
    pub ports_indexed: usize,
    pub options_indexed: usize,
    pub edges_indexed: usize,
    pub unresolved: usize,
    /// Ports whose `make` invocation failed outright.
    pub failed: usize,
    /// Ports skipped because their Makefiles had not changed.
    pub cached: usize,
}

/// Directories under the ports root that are not categories.
const NON_CATEGORIES: [&str; 6] = [
    "Mk",
    "Templates",
    "Tools",
    "Keywords",
    "distfiles",
    "packages",
];

/// Every `category/port` directory holding a Makefile.
///
/// Enumeration stays a directory walk rather than something make reports: a
/// dependency may name a port that cannot itself be evaluated, and the edge to
/// it is still real. Having a row for every directory is what lets those edges
/// exist without inventing a target.
pub fn enumerate_ports(ports_dir: &Path) -> Result<Vec<String>> {
    let mut origins = Vec::new();

    let categories = fs::read_dir(ports_dir)
        .with_context(|| format!("cannot read ports tree at {ports_dir:?}"))?;

    for category in categories.flatten() {
        let cat_name = category.file_name().to_string_lossy().to_string();
        if cat_name.starts_with('.') || NON_CATEGORIES.contains(&cat_name.as_str()) {
            continue;
        }
        if !category.path().is_dir() {
            continue;
        }

        let Ok(ports) = fs::read_dir(category.path()) else {
            continue;
        };
        for port in ports.flatten() {
            let port_name = port.file_name().to_string_lossy().to_string();
            if port_name.starts_with('.') {
                continue;
            }
            if port.path().join("Makefile").is_file() {
                origins.push(format!("{cat_name}/{port_name}"));
            }
        }
    }

    origins.sort();
    Ok(origins)
}

/// Writes one port's facts, given the origin→id map built by the first pass.
///
/// An edge whose target is not in the map is recorded as unresolved rather than
/// skipped. The map holds every directory in the tree, so reaching this means
/// the port genuinely named something that does not exist.
fn store_facts(
    tx: &Connection,
    facts: &PortFacts,
    ids: &HashMap<String, i64>,
    stats: &mut IndexerStats,
) -> Result<()> {
    let Some(&port_id) = ids.get(&facts.origin) else {
        return Ok(());
    };

    tx.execute(
        "UPDATE ports SET pkgbase = ?2, pkgname = ?3, resolved = 1 WHERE id = ?1",
        params![port_id, facts.pkgbase, facts.pkgname],
    )?;

    for flavour in &facts.flavours {
        tx.execute(
            "INSERT OR REPLACE INTO port_flavour (port_id, flavour, pkgname) VALUES (?1, ?2, '')",
            params![port_id, flavour],
        )?;
    }

    let mut option_ids: HashMap<&str, i64> = HashMap::new();
    for opt in &facts.options {
        tx.execute(
            "INSERT OR REPLACE INTO options
             (port_id, name, description, group_type, group_name, default_on)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                port_id,
                opt.name,
                opt.description,
                opt.group_type,
                opt.group_name,
                opt.default_on as i32,
            ],
        )?;
        let option_id = tx.last_insert_rowid();
        option_ids.insert(opt.name.as_str(), option_id);
        stats.options_indexed += 1;

        for implied in &opt.implies {
            tx.execute(
                "INSERT OR REPLACE INTO option_implies (option_id, implies_name) VALUES (?1, ?2)",
                params![option_id, implied],
            )?;
        }
        for prevented in &opt.prevents {
            tx.execute(
                "INSERT OR REPLACE INTO option_prevents (option_id, prevents_name) VALUES (?1, ?2)",
                params![option_id, prevented],
            )?;
        }
    }

    for dep in &facts.deps {
        let Some(&to_id) = ids.get(&dep.origin) else {
            tx.execute(
                "INSERT INTO unresolved_dep (port_origin, raw_entry, reason) VALUES (?1, ?2, ?3)",
                params![facts.origin, dep.origin, REASON_NO_SUCH_PORT],
            )?;
            stats.unresolved += 1;
            continue;
        };

        // A port depending on itself is how several ports express "build the
        // other flavour of me"; it is not a cycle worth recording.
        if to_id == port_id && dep.flavour.is_none() {
            continue;
        }

        let via_id = dep.via_option.as_deref().and_then(|o| option_ids.get(o));
        tx.execute(
            "INSERT INTO dep_edge
             (from_port_id, to_port_id, to_flavour, class, via_option_id, polarity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                port_id,
                to_id,
                dep.flavour,
                dep.class,
                via_id,
                dep.polarity.as_str(),
            ],
        )?;
        stats.edges_indexed += 1;
    }

    for un in &facts.unresolved {
        tx.execute(
            "INSERT INTO unresolved_dep (port_origin, raw_entry, reason) VALUES (?1, ?2, ?3)",
            params![facts.origin, un.raw_entry, un.reason],
        )?;
        stats.unresolved += 1;
    }

    Ok(())
}

/// Origins whose cached facts are stale, judged by Makefile mtime.
fn stale_origins(conn: &Connection, env: &MakeEnv, origins: &[String]) -> (Vec<String>, usize) {
    let mut cached_mtimes: HashMap<String, i64> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT origin, source_mtime FROM port_mtime") {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        {
            for row in rows.flatten() {
                cached_mtimes.insert(row.0, row.1);
            }
        }
    }

    let mut stale = Vec::new();
    let mut fresh = 0;
    for origin in origins {
        let on_disk = resolve::newest_makefile_mtime(&env.ports_dir.join(origin));
        match (cached_mtimes.get(origin), on_disk) {
            (Some(&cached), Some(now)) if cached == now => fresh += 1,
            _ => stale.push(origin.clone()),
        }
    }
    (stale, fresh)
}

/// Rebuilds the cache from the ports tree.
pub fn index_ports_dir(conn: &mut Connection, env: &MakeEnv) -> Result<IndexerStats> {
    let mut stats = IndexerStats {
        ports_indexed: 0,
        options_indexed: 0,
        edges_indexed: 0,
        unresolved: 0,
        failed: 0,
        cached: 0,
    };

    let origins = enumerate_ports(&env.ports_dir)?;
    stats.ports_indexed = origins.len();

    // Pass 1: a row for every port, so every edge has something to point at.
    {
        let tx = conn.transaction()?;
        for origin in &origins {
            tx.execute(
                "INSERT OR IGNORE INTO ports (origin) VALUES (?1)",
                params![origin],
            )?;
        }
        tx.commit()?;
    }

    let ids: HashMap<String, i64> = {
        let mut stmt = conn.prepare("SELECT origin, id FROM ports")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.flatten().collect()
    };

    let (stale, fresh) = stale_origins(conn, env, &origins);
    stats.cached = fresh;

    if stale.is_empty() {
        return Ok(stats);
    }

    // Pass 2: evaluate. One `make` per port, which is the whole cost of an index.
    let resolved: Vec<(String, Result<PortFacts>)> = stale
        .par_iter()
        .map(|origin| (origin.clone(), resolve::resolve_one(env, origin)))
        .collect();

    // A tree make cannot read fails identically for every port in it, so only
    // the first few are worth printing.
    const MAX_REPORTED: usize = 5;
    let mut reported = 0;

    let tx = conn.transaction()?;
    for (origin, result) in &resolved {
        // Re-resolving a port replaces what it said last time; anything keyed on
        // it goes first so a shrinking option list cannot leave orphans.
        let id = ids.get(origin).copied().unwrap_or(-1);
        tx.execute("DELETE FROM dep_edge WHERE from_port_id = ?1", params![id])?;
        tx.execute("DELETE FROM options WHERE port_id = ?1", params![id])?;
        tx.execute("DELETE FROM port_flavour WHERE port_id = ?1", params![id])?;
        tx.execute(
            "DELETE FROM unresolved_dep WHERE port_origin = ?1",
            params![origin],
        )?;

        match result {
            Ok(facts) => {
                store_facts(&tx, facts, &ids, &mut stats)?;
                tx.execute(
                    "INSERT OR REPLACE INTO port_mtime (origin, source_mtime) VALUES (?1, ?2)",
                    params![origin, facts.source_mtime],
                )?;
            }
            Err(e) => {
                if reported < MAX_REPORTED {
                    eprintln!("[!] {e}");
                    reported += 1;
                }
                tx.execute(
                    "INSERT INTO unresolved_dep (port_origin, raw_entry, reason)
                     VALUES (?1, '', 'EVAL_FAILED')",
                    params![origin],
                )?;
                stats.failed += 1;
            }
        }
    }
    tx.commit()?;

    if stats.failed > reported {
        eprintln!(
            "[!] ...and {} more could not be evaluated",
            stats.failed - reported
        );
    }

    crate::db::set_meta(conn, "ports_dir", &env.ports_dir.to_string_lossy())?;
    crate::db::set_meta(conn, "resolved_as", &env.describe_target())?;

    Ok(stats)
}
