//! The cache, which is a memo and nothing more.
//!
//! Earlier versions modelled the ports tree here: tables for ports, options,
//! dependency edges, implications, flavours. That could not be made right. A
//! port's dependencies are a function of its options — `MYSQL_USES=mysql` and
//! `.if ${PORT_OPTIONS:MFOO}` blocks produce nothing unless the option is set
//! while make reads the Makefile — so describing a port takes one row per option
//! set, and there are 2^n of those.
//!
//! So nothing about the ports domain is stored any more. [`crate::resolve`] asks
//! make a question and gets a reply; this remembers the reply against the
//! question, and [`crate::resolve::parse_reply`] — which is pure — turns it into
//! facts on the way out. Staleness stops being a concept, because everything
//! that could make an answer wrong is *in the key*: which port, resolved as
//! what, from Makefiles of what age, under which options.

use anyhow::Result;
use rusqlite::{params, Connection};

/// Bumped when the memo's shape changes. A miss only costs an evaluation, so
/// there is nothing to migrate — the table is dropped and refills itself.
const CURRENT_SCHEMA_VERSION: i32 = 9;

/// Per-connection tuning, run on every connection that touches the memo.
///
/// `journal_mode = WAL` lives in the database file, but `busy_timeout` and
/// `synchronous` die with the connection that set them — so setting them in
/// [`init_db`] alone tunes only the connection that created the schema, and
/// every parallel worker after it would get an immediate `SQLITE_BUSY` instead
/// of waiting its turn for the write lock.
pub fn tune_connection(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA busy_timeout = 5000;
        ",
    )?;
    Ok(())
}

/// Opens the memo, creating or resetting it as needed.
pub fn init_db(conn: &Connection, force_reset: bool) -> Result<()> {
    let on_disk_version: i32 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap_or(0);

    if force_reset || on_disk_version != CURRENT_SCHEMA_VERSION {
        conn.execute_batch(
            "
            DROP TABLE IF EXISTS reply;
            -- The tables that used to model the tree, dropped children-first
            DROP TABLE IF EXISTS port_files;
            DROP TABLE IF EXISTS port_conflicts;
            DROP TABLE IF EXISTS port_details;
            DROP TABLE IF EXISTS port_mtime;
            DROP TABLE IF EXISTS unresolved_dep;
            DROP TABLE IF EXISTS dep_edge;
            DROP TABLE IF EXISTS option_implies;
            DROP TABLE IF EXISTS option_prevents;
            DROP TABLE IF EXISTS port_deps;
            DROP TABLE IF EXISTS option_deps;
            DROP TABLE IF EXISTS port_flavour;
            DROP TABLE IF EXISTS options;
            DROP TABLE IF EXISTS ports;
            DROP TABLE IF EXISTS meta;
            ",
        )?;
    }

    tune_connection(conn)?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;

        -- One make reply, against the question that produced it.
        --
        -- `target` is what the port was resolved as (ARCH, OSVERSION, ...),
        -- because COMPLETE_OPTIONS_LIST varies with it. `mtime` is the newest of
        -- the port's Makefiles, so a tree update simply misses. `options_key` is
        -- the option set it was evaluated under, sorted, or empty for 'as the
        -- port ships'.
        --
        -- WAL and a busy timeout because the walk evaluates in parallel and each
        -- worker writes through its own connection.
        CREATE TABLE IF NOT EXISTS reply (
            origin      TEXT NOT NULL,
            target      TEXT NOT NULL,
            mtime       INTEGER NOT NULL,
            options_key TEXT NOT NULL,
            reply       TEXT NOT NULL,
            PRIMARY KEY (origin, target, mtime, options_key)
        ) WITHOUT ROWID;
        ",
    )?;

    conn.execute(
        &format!("PRAGMA user_version = {};", CURRENT_SCHEMA_VERSION),
        [],
    )?;

    Ok(())
}

/// A remembered reply, if this exact question has been asked before.
pub fn get_reply(
    conn: &Connection,
    origin: &str,
    target: &str,
    mtime: i64,
    options_key: &str,
) -> Option<String> {
    conn.query_row(
        "SELECT reply FROM reply
         WHERE origin = ?1 AND target = ?2 AND mtime = ?3 AND options_key = ?4",
        params![origin, target, mtime, options_key],
        |row| row.get(0),
    )
    .ok()
}

/// Remembers a reply, forgetting what this port said when its Makefiles were
/// older.
///
/// Without that second step the memo would keep every answer the port has ever
/// given across every tree update, and only the newest is reachable — the age is
/// part of the key.
pub fn put_reply(
    conn: &Connection,
    origin: &str,
    target: &str,
    mtime: i64,
    options_key: &str,
    reply: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM reply WHERE origin = ?1 AND target = ?2 AND mtime <> ?3",
        params![origin, target, mtime],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO reply (origin, target, mtime, options_key, reply)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![origin, target, mtime, options_key, reply],
    )?;
    Ok(())
}

/// How many replies are remembered, for the preheat summary.
pub fn reply_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM reply", [], |row| row.get(0))
        .unwrap_or(0)
}
