use anyhow::Result;
use rusqlite::Connection;

/// Bumped whenever an existing cache would be *wrong* or *incomplete*, not only
/// when the table layout changes.
///
/// 8 replaces the tables the regex sweep filled. Dependency targets are no
/// longer strings that might name a port — they are foreign keys to one, so a
/// dangling edge is unrepresentable rather than pruned afterwards. Options carry
/// their real grouping and implications, and anything that could not be resolved
/// is recorded in `unresolved_dep` instead of being dropped.
const CURRENT_SCHEMA_VERSION: i32 = 8;

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

        // Dropped children-first: the edge tables hold the foreign keys.
        conn.execute_batch(
            "
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

    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA busy_timeout = 5000;
        PRAGMA foreign_keys = ON;

        -- `id` exists so edges can reference a port rather than name one.
        -- `resolved` distinguishes a port make could evaluate from one that only
        -- exists as a directory; an edge may point at either, and the difference
        -- is what tells a missing dependency from an unbuildable one.
        CREATE TABLE IF NOT EXISTS ports (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            origin TEXT NOT NULL UNIQUE,
            pkgbase TEXT NOT NULL DEFAULT '',
            pkgname TEXT NOT NULL DEFAULT '',
            resolved INTEGER NOT NULL DEFAULT 0
        );

        -- A flavoured port builds several packages from one directory, each with
        -- its own PKGNAME. Dependencies name them with an `@flavour` suffix.
        --
        -- Flavours are recorded but do not become separate nodes, because
        -- options are not per-flavour: `bsd.options.mk:182` keys the options
        -- file on `OPTIONS_NAME`, which defaults to PKGORIGIN with the slash
        -- turned into an underscore — so py-setuptools@py311 and @py312 both
        -- read and write
        -- `devel_py-setuptools/options`. Configuring per flavour would write a
        -- file the framework never reads.
        CREATE TABLE IF NOT EXISTS port_flavour (
            port_id INTEGER NOT NULL REFERENCES ports(id) ON DELETE CASCADE,
            flavour TEXT NOT NULL,
            pkgname TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (port_id, flavour)
        );

        CREATE TABLE IF NOT EXISTS options (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            port_id INTEGER NOT NULL REFERENCES ports(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            group_type TEXT NOT NULL DEFAULT 'DEFINE',
            group_name TEXT NOT NULL DEFAULT '',
            default_on INTEGER NOT NULL DEFAULT 0,
            UNIQUE(port_id, name)
        );

        -- FOO_IMPLIES / FOO_PREVENTS. Stored by name rather than by option id
        -- because a port may name an option it does not itself define.
        CREATE TABLE IF NOT EXISTS option_implies (
            option_id INTEGER NOT NULL REFERENCES options(id) ON DELETE CASCADE,
            implies_name TEXT NOT NULL,
            PRIMARY KEY (option_id, implies_name)
        );

        CREATE TABLE IF NOT EXISTS option_prevents (
            option_id INTEGER NOT NULL REFERENCES options(id) ON DELETE CASCADE,
            prevents_name TEXT NOT NULL,
            PRIMARY KEY (option_id, prevents_name)
        );

        -- One row per resolved dependency.
        --
        -- `to_port_id` is a foreign key, so an edge pointing at nothing cannot
        -- be stored at all. That is the whole point of the table: the previous
        -- schema held target *strings*, wrote whatever a regex produced, and
        -- deleted the ones that named no port afterwards.
        --
        -- `polarity` carries the `_OFF` forms, which apply when the option is
        -- unset and were invisible to the old sweep.
        CREATE TABLE IF NOT EXISTS dep_edge (
            from_port_id  INTEGER NOT NULL REFERENCES ports(id) ON DELETE CASCADE,
            to_port_id    INTEGER NOT NULL REFERENCES ports(id) ON DELETE CASCADE,
            to_flavour    TEXT,
            class         TEXT NOT NULL,
            via_option_id INTEGER REFERENCES options(id) ON DELETE CASCADE,
            polarity      TEXT NOT NULL DEFAULT 'ON'
        );

        -- Depends entries that named nothing this cache can point at, kept so
        -- that `SELECT COUNT(*) FROM unresolved_dep` answers 'did anything fail
        -- to resolve' instead of it being a claim.
        CREATE TABLE IF NOT EXISTS unresolved_dep (
            port_origin TEXT NOT NULL,
            raw_entry   TEXT NOT NULL,
            reason      TEXT NOT NULL
        );

        -- When each port's Makefiles were last seen, so re-indexing after a
        -- tree update re-evaluates only what changed. Keyed by origin rather
        -- than port id so a row survives the port table being rebuilt.
        CREATE TABLE IF NOT EXISTS port_mtime (
            origin TEXT PRIMARY KEY,
            source_mtime INTEGER NOT NULL
        );

        -- Small key/value store. Holds the ports tree path used at index time so
        -- later runs can find the tree again without being told twice, and the
        -- ARCH/OSVERSION the cache was resolved as.
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_options_port ON options(port_id);
        CREATE INDEX IF NOT EXISTS idx_edge_from ON dep_edge(from_port_id);
        CREATE INDEX IF NOT EXISTS idx_edge_via ON dep_edge(via_option_id);
        CREATE INDEX IF NOT EXISTS idx_flavour_port ON port_flavour(port_id);
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
