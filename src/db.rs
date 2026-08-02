use anyhow::Result;
use rusqlite::Connection;

const CURRENT_SCHEMA_VERSION: i32 = 6;

/// Initializes the SQLite database schema.
/// Drops outdated tables if the schema version on disk is incompatible
/// or if `force_reset` is explicitly requested.
pub fn init_db(conn: &Connection, force_reset: bool) -> Result<()> {
    let on_disk_version: i32 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap_or(0);

    if force_reset || on_disk_version != CURRENT_SCHEMA_VERSION {
        if on_disk_version != CURRENT_SCHEMA_VERSION && !force_reset {
            // The cache is dropped, not repopulated — re-reading the ports tree
            // is what `bgone index` does, and it needs a path to read it from.
            println!(
                "[*] Detected incompatible database schema (v{} -> v{}). Discarding the cache; \
                 re-run 'bgone index' to rebuild it.",
                on_disk_version, CURRENT_SCHEMA_VERSION
            );
        }

        conn.execute_batch(
            "
            DROP TABLE IF EXISTS port_files;
            DROP TABLE IF EXISTS port_conflicts;
            DROP TABLE IF EXISTS port_details;
            DROP TABLE IF EXISTS port_deps;
            DROP TABLE IF EXISTS option_deps;
            DROP TABLE IF EXISTS options;
            DROP TABLE IF EXISTS ports;
            DROP TABLE IF EXISTS meta;
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

        -- Dependencies a port pulls in regardless of which options are set,
        -- i.e. the plain {PKG,EXTRACT,PATCH,FETCH,BUILD,LIB,RUN,TEST}_DEPENDS
        -- that make up _UNIFIED_DEPENDS. `poudriere options` recurses over
        -- these as well as the option-conditional ones in option_deps.
        CREATE TABLE IF NOT EXISTS port_deps (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            port_origin TEXT NOT NULL,
            dep_origin TEXT NOT NULL,
            dep_type TEXT NOT NULL,
            FOREIGN KEY(port_origin) REFERENCES ports(origin) ON DELETE CASCADE,
            UNIQUE(port_origin, dep_origin)
        );

        -- What the ports tree itself reports for a port, via `make describe-json`.
        -- The regex sweep cannot see options a port inherits from a MASTERDIR
        -- slave relationship or from Mk/Uses machinery, nor can it reconstruct
        -- PKGNAME, so these are read from the tree for the ports being
        -- configured and cached until their Makefiles change.
        CREATE TABLE IF NOT EXISTS port_details (
            port_origin TEXT PRIMARY KEY,
            pkgbase TEXT NOT NULL,
            pkgname TEXT NOT NULL,
            complete_options_list TEXT NOT NULL,
            options_default TEXT NOT NULL,
            source_mtime INTEGER NOT NULL
        );

        -- Small key/value store. Holds the ports tree path used at index time so
        -- later runs can find the tree again without being told twice.
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );



        CREATE INDEX IF NOT EXISTS idx_options_port ON options(port_origin);
        CREATE INDEX IF NOT EXISTS idx_deps_port_opt ON option_deps(port_origin, option_name);
        CREATE INDEX IF NOT EXISTS idx_port_deps_port ON port_deps(port_origin);
        ",
    )?;

    conn.execute(
        &format!("PRAGMA user_version = {};", CURRENT_SCHEMA_VERSION),
        [],
    )?;

    Ok(())
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![key],
        |row| row.get(0),
    )
    .ok()
}
