use anyhow::Result;
use rusqlite::Connection;

const CURRENT_SCHEMA_VERSION: i32 = 2;

/// Initializes the SQLite database schema.
/// Drops outdated tables if the schema version on disk is incompatible
/// or if `force_reset` is explicitly requested.
pub fn init_db(conn: &Connection, force_reset: bool) -> Result<()> {
    let on_disk_version: i32 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap_or(0);

    if force_reset || on_disk_version != CURRENT_SCHEMA_VERSION {
        if on_disk_version != CURRENT_SCHEMA_VERSION && !force_reset {
            println!(
                "[*] Detected incompatible database schema (v{} -> v{}). Rebuilding database cache...",
                on_disk_version, CURRENT_SCHEMA_VERSION
            );
        }

        conn.execute_batch(
            "
            DROP TABLE IF EXISTS option_deps;
            DROP TABLE IF EXISTS options;
            DROP TABLE IF EXISTS ports;
            ",
        )?;
    }

    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA busy_timeout = 5000;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS ports (
            origin TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            comment TEXT
        );

        CREATE TABLE IF NOT EXISTS options (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            port_origin TEXT NOT NULL,
            option_name TEXT NOT NULL,
            default_state INTEGER NOT NULL,
            description TEXT,
            group_type TEXT DEFAULT 'DEFINE',
            group_name TEXT DEFAULT '',
            FOREIGN KEY(port_origin) REFERENCES ports(origin) ON DELETE CASCADE,
            UNIQUE(port_origin, option_name)
        );

        CREATE TABLE IF NOT EXISTS option_deps (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            port_origin TEXT NOT NULL,
            option_name TEXT NOT NULL,
            dep_origin TEXT NOT NULL,
            dep_type TEXT NOT NULL,
            FOREIGN KEY(port_origin) REFERENCES ports(origin) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_options_port ON options(port_origin);
        CREATE INDEX IF NOT EXISTS idx_deps_port_opt ON option_deps(port_origin, option_name);
        ",
    )?;

    conn.execute(
        &format!("PRAGMA user_version = {};", CURRENT_SCHEMA_VERSION),
        [],
    )?;

    Ok(())
}
