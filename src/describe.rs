//! Reads authoritative port metadata out of the ports tree.
//!
//! [`crate::indexer`] parses Makefiles with regexes, which is fast enough to
//! sweep 35,000 ports but cannot evaluate them. Measured against a current tree,
//! that leaves one gap worth closing and one that mostly is not:
//!
//! * **Option lists the sweep cannot see.** `config-conditional` re-opens the
//!   dialog whenever a port defines an option the saved file does not name, so a
//!   missed option means poudriere keeps prompting for that port. This is the
//!   reason the pass exists — but it is a narrow one. Of 264 sampled ports the
//!   sweep found options for, the tree agreed exactly on 261, and in *no* case
//!   did the tree know an option the sweep had missed. Of 234 ports the sweep
//!   found none for, only 2 actually had any — both inheriting through
//!   `MASTERDIR`, as `security/ossec-hids-agent` does. Call it 1% of ports.
//! * **`PKGNAME`, wrong for 88% of ports** — no `PORTREVISION`/`PORTEPOCH`, no
//!   `USES`-synthesised prefix (`py312-`, `rubygem-`), and no version at all for
//!   roughly a third of the tree. Only `_OPTIONS_READ` consumes it, and the ports
//!   framework writes that header without ever reading it back, so this is
//!   fidelity rather than function.
//!
//! (The three cases where the sweep claimed options the tree does not define are
//! harmless: `bsd.options.mk` guards `OPTIONS_FILE_SET` with a membership test,
//! and an `OPTIONS_FILE_UNSET` naming a non-member is a no-op.)
//!
//! `describe-json` reports both, but forks `make` per port.
//!
//! `bsd.port.mk` is enormous, which is why building a full `INDEX` takes tens of
//! minutes. So it is run only for the ports actually being configured, and
//! cached until a port's Makefiles change: measured at ~1,450 ports in 74s cold
//! and 0.36s warm.

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

pub struct DescribeStats {
    pub described: usize,
    pub cached: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortDetails {
    pub origin: String,
    pub pkgbase: String,
    pub pkgname: String,
    /// Every option the port defines — `COMPLETE_OPTIONS_LIST`, the list
    /// `config-conditional` compares a saved options file against.
    pub complete_options_list: Vec<String>,
    /// Of those, the ones on by default.
    pub options_default: Vec<String>,
    pub source_mtime: i64,
}

/// Newest mtime across a port's `Makefile*`, used to tell when cached details
/// have gone stale.
fn newest_makefile_mtime(port_dir: &Path) -> Option<i64> {
    let mut newest = None;
    for entry in fs::read_dir(port_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("Makefile") {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        newest = Some(newest.map_or(mtime, |n: i64| n.max(mtime)));
    }
    newest
}

/// `${VAR:ts,:Q:S/,/","/g}` renders an empty make variable as a single empty
/// string rather than an empty list, so those have to be dropped.
fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Pulls the fields we use out of one `describe-json` document.
///
/// A port with `FLAVORS` emits an object keyed by `"<flavor>-<portdir>"` with
/// one document per flavour; anything else emits the document directly. The
/// first flavour is taken, matching what `poudriere options` configures when no
/// flavour is named.
pub fn parse_describe_json(text: &str, origin: &str, source_mtime: i64) -> Result<PortDetails> {
    let root: Value = serde_json::from_str(text)
        .with_context(|| format!("{origin}: unparseable describe-json"))?;

    let obj = root
        .as_object()
        .with_context(|| format!("{origin}: describe-json was not an object"))?;

    let doc = if obj.contains_key("pkgname") {
        &root
    } else {
        obj.values()
            .find(|v| v.get("pkgname").is_some())
            .with_context(|| format!("{origin}: describe-json had no pkgname"))?
    };

    let field = |name: &str| doc.get(name).and_then(|v| v.as_str()).unwrap_or("").trim();

    let pkgname = field("pkgname");
    if pkgname.is_empty() {
        bail!("{origin}: describe-json reported an empty pkgname");
    }
    let pkgbase = match field("pkgbase") {
        "" => pkgname
            .rsplit_once('-')
            .map(|(base, _)| base)
            .unwrap_or(pkgname),
        base => base,
    };

    Ok(PortDetails {
        origin: origin.to_string(),
        pkgbase: pkgbase.to_string(),
        pkgname: pkgname.to_string(),
        complete_options_list: string_list(doc.get("complete_options_list")),
        options_default: string_list(doc.get("options_default")),
        source_mtime,
    })
}

fn describe_one(ports_dir: &Path, origin: &str) -> Result<PortDetails> {
    let port_dir = ports_dir.join(origin);
    let source_mtime = newest_makefile_mtime(&port_dir)
        .with_context(|| format!("{origin}: no Makefile under {port_dir:?}"))?;

    let output = Command::new("make")
        .arg("-C")
        .arg(&port_dir)
        .arg("describe-json")
        .output()
        .with_context(|| format!("{origin}: could not run make"))?;

    if !output.status.success() {
        bail!(
            "{origin}: make describe-json failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    parse_describe_json(
        &String::from_utf8_lossy(&output.stdout),
        origin,
        source_mtime,
    )
}

pub fn load_cached(conn: &Connection, origin: &str) -> Option<PortDetails> {
    conn.query_row(
        "SELECT pkgbase, pkgname, complete_options_list, options_default, source_mtime
         FROM port_details WHERE port_origin = ?1",
        params![origin],
        |row| {
            Ok(PortDetails {
                origin: origin.to_string(),
                pkgbase: row.get(0)?,
                pkgname: row.get(1)?,
                complete_options_list: split_stored(&row.get::<_, String>(2)?),
                options_default: split_stored(&row.get::<_, String>(3)?),
                source_mtime: row.get(4)?,
            })
        },
    )
    .ok()
}

fn split_stored(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
}

fn store(conn: &Connection, details: &PortDetails) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO port_details
         (port_origin, pkgbase, pkgname, complete_options_list, options_default, source_mtime)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            details.origin,
            details.pkgbase,
            details.pkgname,
            details.complete_options_list.join(" "),
            details.options_default.join(" "),
            details.source_mtime,
        ],
    )?;
    Ok(())
}

/// Brings `port_details` up to date for `origins`.
///
/// Ports whose cached entry still matches their Makefiles on disk are left
/// alone. A port that cannot be described keeps whatever the regex sweep found
/// for it, so a tree that `make` cannot read degrades rather than failing.
pub fn describe_ports(
    conn: &mut Connection,
    ports_dir: &Path,
    origins: &[String],
) -> Result<DescribeStats> {
    let mut stats = DescribeStats {
        described: 0,
        cached: 0,
        failed: 0,
    };

    let mut stale: Vec<(String, PathBuf)> = Vec::new();
    for origin in origins {
        let port_dir = ports_dir.join(origin);
        let on_disk = newest_makefile_mtime(&port_dir);
        match (load_cached(conn, origin), on_disk) {
            (Some(cached), Some(mtime)) if cached.source_mtime == mtime => stats.cached += 1,
            (_, Some(_)) => stale.push((origin.clone(), port_dir)),
            // No Makefile to read: nothing to do beyond what is already cached
            (_, None) => stats.failed += 1,
        }
    }

    if stale.is_empty() {
        return Ok(stats);
    }

    let described: Vec<Result<PortDetails>> = stale
        .par_iter()
        .map(|(origin, _)| describe_one(ports_dir, origin))
        .collect();

    // A tree `make` cannot read fails identically for every port in it, so only
    // the first few are worth printing.
    const MAX_REPORTED: usize = 5;
    let mut reported = 0;

    let tx = conn.transaction()?;
    for result in described {
        match result {
            Ok(details) => {
                store(&tx, &details)?;
                stats.described += 1;
            }
            Err(e) => {
                if reported < MAX_REPORTED {
                    eprintln!("[!] {e}");
                    reported += 1;
                }
                stats.failed += 1;
            }
        }
    }
    tx.commit()?;

    if stats.failed > reported {
        eprintln!(
            "[!] ...and {} more could not be read",
            stats.failed - reported
        );
    }

    Ok(stats)
}
