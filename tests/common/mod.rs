#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// RAII wrapper to guarantee test directories and files are cleaned up on completion or panic.
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let count = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bgone_test_{}_{}_{}",
            prefix,
            std::process::id(),
            count
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("Failed to create temporary test directory");
        Self { path }
    }

    /// Absolute path of `name` inside this directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Builds a minimal ports tree containing a single port and returns its root.
pub fn write_mock_ports_tree(temp: &TempDir) -> PathBuf {
    let ports_root = temp.join("ports");
    let nginx_dir = ports_root.join("www").join("nginx");
    fs::create_dir_all(&nginx_dir).unwrap();
    fs::write(
        nginx_dir.join("Makefile"),
        "PORTNAME=   nginx\n\
         PORTVERSION=1.24.0\n\
         COMMENT=    Robust HTTP and reverse proxy server\n\
         OPTIONS_DEFINE=   HTTP2 DOCS\n\
         OPTIONS_DEFAULT=  HTTP2\n",
    )
    .unwrap();
    ports_root
}

// ---------------------------------------------------------------- cache fixtures
//
// The cache is written by `bgone index`, which evaluates every port with make.
// Tests that only care about the graph should not have to run make, so these
// write the same rows directly. They are the only place that knows the schema's
// shape, so a schema change lands here rather than in forty call sites.

use rusqlite::Connection;

/// Inserts a port and returns its id.
pub fn add_port(conn: &Connection, origin: &str) -> i64 {
    let pkgname = format!("{}-1.0", origin.split('/').nth(1).unwrap_or(origin));
    conn.execute(
        "INSERT OR IGNORE INTO ports (origin, pkgbase, pkgname, resolved)
         VALUES (?1, ?2, ?3, 1)",
        rusqlite::params![origin, origin.split('/').nth(1).unwrap_or(origin), pkgname],
    )
    .unwrap();
    port_id(conn, origin)
}

pub fn port_id(conn: &Connection, origin: &str) -> i64 {
    conn.query_row(
        "SELECT id FROM ports WHERE origin = ?1",
        rusqlite::params![origin],
        |r| r.get(0),
    )
    .unwrap_or_else(|_| panic!("no port row for {origin}"))
}

/// Inserts an option on a port, creating the port if needed. Returns its id.
#[allow(clippy::too_many_arguments)]
pub fn add_option(
    conn: &Connection,
    origin: &str,
    name: &str,
    default_on: bool,
    description: &str,
    group_type: &str,
    group_name: &str,
) -> i64 {
    let pid = add_port(conn, origin);
    conn.execute(
        "INSERT OR REPLACE INTO options
         (port_id, name, description, group_type, group_name, default_on)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            pid,
            name,
            description,
            group_type,
            group_name,
            default_on as i32
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

pub fn option_id(conn: &Connection, origin: &str, name: &str) -> i64 {
    conn.query_row(
        "SELECT o.id FROM options o JOIN ports p ON p.id = o.port_id
         WHERE p.origin = ?1 AND o.name = ?2",
        rusqlite::params![origin, name],
        |r| r.get(0),
    )
    .unwrap_or_else(|_| panic!("no option {name} on {origin}"))
}

/// An edge that applies only when `opt` on `from` is set.
pub fn add_option_dep(conn: &Connection, from: &str, opt: &str, to: &str) {
    add_option_dep_with(conn, from, opt, to, "RUN", "ON");
}

pub fn add_option_dep_with(
    conn: &Connection,
    from: &str,
    opt: &str,
    to: &str,
    class: &str,
    polarity: &str,
) {
    let from_id = add_port(conn, from);
    let to_id = add_port(conn, to);
    let opt_id = conn
        .query_row(
            "SELECT o.id FROM options o WHERE o.port_id = ?1 AND o.name = ?2",
            rusqlite::params![from_id, opt],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or_else(|_| add_option(conn, from, opt, true, "", "DEFINE", ""));

    conn.execute(
        "INSERT INTO dep_edge (from_port_id, to_port_id, class, via_option_id, polarity)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![from_id, to_id, class, opt_id, polarity],
    )
    .unwrap();
}

/// An edge that applies whatever the options say.
pub fn add_port_dep(conn: &Connection, from: &str, to: &str) {
    add_port_dep_with(conn, from, to, "LIB");
}

pub fn add_port_dep_with(conn: &Connection, from: &str, to: &str, class: &str) {
    let from_id = add_port(conn, from);
    let to_id = add_port(conn, to);
    conn.execute(
        "INSERT INTO dep_edge (from_port_id, to_port_id, class, via_option_id)
         VALUES (?1, ?2, ?3, NULL)",
        rusqlite::params![from_id, to_id, class],
    )
    .unwrap();
}

pub fn add_implies(conn: &Connection, origin: &str, opt: &str, implies: &str) {
    let id = option_id(conn, origin, opt);
    conn.execute(
        "INSERT OR REPLACE INTO option_implies (option_id, implies_name) VALUES (?1, ?2)",
        rusqlite::params![id, implies],
    )
    .unwrap();
}

pub fn add_prevents(conn: &Connection, origin: &str, opt: &str, prevents: &str) {
    let id = option_id(conn, origin, opt);
    conn.execute(
        "INSERT OR REPLACE INTO option_prevents (option_id, prevents_name) VALUES (?1, ?2)",
        rusqlite::params![id, prevents],
    )
    .unwrap();
}

/// Sets the package name make would have reported.
pub fn set_pkgname(conn: &Connection, origin: &str, pkgname: &str) {
    add_port(conn, origin);
    conn.execute(
        "UPDATE ports SET pkgname = ?2 WHERE origin = ?1",
        rusqlite::params![origin, pkgname],
    )
    .unwrap();
}

/// A port that exists in the tree but that `make` could not evaluate.
///
/// The indexer inserts a row for every directory so that edges have something
/// to point at, and marks it resolved only once make has answered for it.
pub fn add_unevaluated_port(conn: &Connection, origin: &str) {
    conn.execute(
        "INSERT OR REPLACE INTO ports (origin, pkgbase, pkgname, resolved)
         VALUES (?1, '', '', 0)",
        rusqlite::params![origin],
    )
    .unwrap();
}

// ------------------------------------------------------------ poudriere fixture
//
// poudriere's state is a file-per-property attribute store, so building one in
// a temp directory reproduces it exactly rather than standing in for it. What
// these write is what `poudriere jail -c` and `poudriere ports -c` write.

/// Creates `<etc>/poudriere.d` and returns the etc path to pass as
/// `--poudriere-etc`.
pub fn poudriere_etc(temp: &TempDir, name: &str) -> PathBuf {
    let etc = temp.join(name);
    fs::create_dir_all(etc.join("poudriere.d")).unwrap();
    etc
}

fn attr(etc: &Path, kind: &str, name: &str, property: &str, value: &str) {
    let dir = etc.join("poudriere.d").join(kind).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(property), format!("{value}\n")).unwrap();
}

/// A jail, with its headers, as `poudriere jail -c` leaves it.
///
/// `arch` is written in poudriere's stored form, `host.target`.
pub fn poudriere_jail(etc: &Path, name: &str, arch: &str, version: &str, freebsd_version: &str) {
    let mnt = etc.join("jails-mnt").join(name);
    fs::create_dir_all(mnt.join("usr/include/sys")).unwrap();
    fs::write(
        mnt.join("usr/include/sys/param.h"),
        format!("#define __FreeBSD_version {freebsd_version}\n"),
    )
    .unwrap();

    attr(etc, "jails", name, "arch", arch);
    attr(etc, "jails", name, "version", version);
    attr(etc, "jails", name, "mnt", &mnt.to_string_lossy());
}

/// A jail whose metadata exists but whose filesystem does not — the shape you
/// get when a dataset is not mounted.
pub fn poudriere_jail_without_headers(etc: &Path, name: &str) {
    attr(etc, "jails", name, "arch", "amd64.amd64");
    attr(etc, "jails", name, "version", "14.4-RELEASE");
    attr(
        etc,
        "jails",
        name,
        "mnt",
        &etc.join("gone").to_string_lossy(),
    );
}

/// A ports tree. Returns the path its `mnt` points at.
pub fn poudriere_tree(etc: &Path, name: &str) -> PathBuf {
    let mnt = etc.join("ports-mnt").join(name);
    fs::create_dir_all(&mnt).unwrap();
    attr(etc, "ports", name, "mnt", &mnt.to_string_lossy());
    mnt
}
