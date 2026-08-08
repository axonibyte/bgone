//! Asks the ports tree what a port is, under a given set of options.
//!
//! Everything that wants to know something about a port goes through here, and
//! everything here is a memoised call to `make`. The tree is the only authority:
//! it is what `poudriere` consults, so answering the same question the same way
//! is what makes the two agree by construction rather than by care.
//!
//! The memo is keyed on the whole question — port, resolution target, Makefile
//! age, option set — so a hit is never a guess and a miss only costs one
//! evaluation. Evaluations run in parallel with a connection per worker; SQLite
//! is in WAL mode with a busy timeout, which is what makes that safe.

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

use crate::db;
use crate::resolve::{self, MakeEnv, PortFacts};

/// The option set to evaluate a port under.
///
/// `AsShipped` asks what the port defines and what the maintainer defaults to —
/// the only question that can be asked before its option list is known. `Exactly`
/// pins `PORT_OPTIONS`, which is what makes dependencies hidden behind `.if`
/// blocks and `${opt}_USES` visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Options {
    AsShipped,
    Exactly(Vec<String>),
}

impl Options {
    /// Every distinct set must have a distinct key, and the same set must always
    /// produce the same one — so it is sorted, and `AsShipped` is the empty
    /// string, which no explicit set can collide with because an explicitly
    /// empty set is written as a single space.
    fn key(&self) -> String {
        match self {
            Options::AsShipped => String::new(),
            Options::Exactly(set) => {
                let mut sorted = set.clone();
                sorted.sort();
                sorted.dedup();
                format!(" {}", sorted.join(" "))
            }
        }
    }

    fn as_slice(&self) -> Option<&[String]> {
        match self {
            Options::AsShipped => None,
            Options::Exactly(set) => Some(set.as_slice()),
        }
    }
}

/// One question: which port, under which options.
pub type Question = (String, Options);

pub struct Oracle {
    env: MakeEnv,
    db_path: PathBuf,
    /// What every reply is resolved as, and part of every memo key.
    target: String,
    /// False when the ports tree could not be read. Memoised replies still
    /// answer; anything else fails, and says why.
    tree_readable: bool,
}

impl Oracle {
    pub fn new(env: MakeEnv, db_path: impl Into<PathBuf>) -> Self {
        let tree_readable = env.ports_dir.join("Mk").join("bsd.port.mk").is_file();
        let target = env.describe_target();
        let db_path = db_path.into();

        // The memo makes itself. Left to the caller, a forgotten `init_db` turned
        // every write into an error and so every port into one make could not
        // read — a whole build set silently reduced to the one port named.
        if let Ok(conn) = Connection::open(&db_path) {
            let _ = db::init_db(&conn, false);
        }

        Self {
            env,
            db_path,
            target,
            tree_readable,
        }
    }

    pub fn ports_dir(&self) -> &Path {
        &self.env.ports_dir
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    /// Whether the tree is there to be asked. A run without one is not fatal —
    /// the memo may still hold every answer needed — but it cannot learn
    /// anything new, and what it could not answer has to be said out loud rather
    /// than written as though it were nothing.
    pub fn tree_readable(&self) -> bool {
        self.tree_readable
    }

    /// Every `category/port` in the tree, for resolving glob patterns.
    pub fn enumerate(&self) -> Result<Vec<String>> {
        resolve::enumerate_ports(&self.env.ports_dir)
    }

    fn open(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("could not open the cache at {:?}", self.db_path))?;
        db::tune_connection(&conn)?;
        Ok(conn)
    }

    /// Answers one question, consulting the tree only on a miss.
    pub fn facts(&self, origin: &str, options: &Options) -> Result<PortFacts> {
        let conn = self.open()?;
        self.facts_with(&conn, origin, options)
    }

    fn facts_with(&self, conn: &Connection, origin: &str, options: &Options) -> Result<PortFacts> {
        let key = options.key();
        let port_dir = self.env.ports_dir.join(origin);

        // The age of the Makefiles is part of the question, so it has to be
        // known before the memo can be consulted. Without a tree there is no
        // age; fall back to whatever age is remembered, which is the only thing
        // that lets a session run from the memo alone.
        let mtime = match resolve::newest_makefile_mtime(&port_dir) {
            Some(mtime) => mtime,
            None if !self.tree_readable => {
                return self.remembered_reply(conn, origin, &key);
            }
            None => {
                anyhow::bail!("{origin}: no Makefile under {port_dir:?}");
            }
        };

        if let Some(reply) = db::get_reply(conn, origin, &self.target, mtime, &key) {
            return resolve::parse_reply(origin, &reply, mtime);
        }

        if !self.tree_readable {
            anyhow::bail!(
                "{origin}: not in the cache, and the ports tree at {:?} cannot be read",
                self.env.ports_dir
            );
        }

        let (reply, mtime) = resolve::evaluate(&self.env, origin, options.as_slice())?;
        // A reply that cannot be parsed is not remembered: storing it would make
        // the failure permanent until the tree changed.
        let facts = resolve::parse_reply(origin, &reply, mtime)?;
        // Remembering is worth attempting but not worth failing over: the
        // evaluation already succeeded, and a busy or read-only cache only
        // costs the next asker a re-evaluation.
        let _ = db::put_reply(conn, origin, &self.target, mtime, &key, &reply);
        Ok(facts)
    }

    /// The newest remembered reply for a port whose Makefiles cannot be reached.
    fn remembered_reply(&self, conn: &Connection, origin: &str, key: &str) -> Result<PortFacts> {
        let row: Option<(i64, String)> = conn
            .query_row(
                "SELECT mtime, reply FROM reply
                 WHERE origin = ?1 AND target = ?2 AND options_key = ?3
                 ORDER BY mtime DESC LIMIT 1",
                rusqlite::params![origin, &self.target, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();

        match row {
            Some((mtime, reply)) => resolve::parse_reply(origin, &reply, mtime),
            None => anyhow::bail!(
                "{origin}: not in the cache, and the ports tree at {:?} cannot be read",
                self.env.ports_dir
            ),
        }
    }

    /// Answers a batch of questions in parallel — one level of a dependency
    /// walk, or a whole preheat.
    ///
    /// Chunked rather than one task per question, so a connection is opened once
    /// per chunk instead of once per port. `rusqlite::Connection` is not `Sync`,
    /// so it cannot simply be shared; opening one each time cost more than the
    /// lookups did once the memo was warm — 940 ports took seven seconds, nearly
    /// all of it in `sqlite3_open`.
    ///
    /// Chunks are small enough that a level of a dozen ports still spreads
    /// across cores, and large enough that the open amortises over a preheat of
    /// thousands.
    pub fn facts_many(&self, want: &[Question]) -> Vec<(String, Result<PortFacts>)> {
        want.par_chunks(Self::chunk_size(want.len(), rayon::current_num_threads()))
            .flat_map(|chunk| {
                let conn = self.open();
                chunk
                    .iter()
                    .map(|(origin, options)| {
                        let answer = match &conn {
                            Ok(conn) => self.facts_with(conn, origin, options),
                            Err(e) => Err(anyhow::anyhow!("{e}")),
                        };
                        (origin.clone(), answer)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// How many questions each worker takes at once: an even split across the
    /// threads, capped so a preheat of thousands still amortises its connection
    /// opens without a level of a dozen ports collapsing onto one core.
    fn chunk_size(len: usize, threads: usize) -> usize {
        const CHUNK: usize = 8;
        len.div_ceil(threads.max(1)).clamp(1, CHUNK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key has to separate every distinct question, and join every identical
    /// one — a set that hashed by insertion order would evaluate the same port
    /// twice and call it a miss.
    #[test]
    fn the_memo_key_is_the_option_set_and_nothing_else() {
        let set = |names: &[&str]| Options::Exactly(names.iter().map(|s| s.to_string()).collect());

        assert_eq!(set(&["SSL", "DOCS"]).key(), set(&["DOCS", "SSL"]).key());
        assert_eq!(set(&["SSL", "SSL"]).key(), set(&["SSL"]).key());
        assert_ne!(set(&["SSL"]).key(), set(&["DOCS"]).key());

        // 'as the port ships' is a different question from 'with nothing set',
        // and they must not share a row
        assert_ne!(Options::AsShipped.key(), set(&[]).key());
        assert_eq!(Options::AsShipped.key(), "");
    }

    /// A dozen ports must spread across a dozen threads, not sit eight-deep on
    /// one core while the rest idle — a cache-miss chunk is a make invocation
    /// per port, and the interactive resettle path feels every serialised one.
    #[test]
    fn small_batches_spread_across_threads() {
        // A level of a dozen on eight threads: two per chunk, all cores busy
        assert_eq!(Oracle::chunk_size(12, 8), 2);
        // Fewer questions than threads: one each
        assert_eq!(Oracle::chunk_size(3, 8), 1);
        // A preheat of thousands still amortises the connection opens
        assert_eq!(Oracle::chunk_size(10_000, 8), 8);
        // Degenerate inputs stay valid chunk sizes
        assert_eq!(Oracle::chunk_size(0, 8), 1);
        assert_eq!(Oracle::chunk_size(5, 0), 5);
    }

    /// `busy_timeout` is a per-connection pragma. Only the connection that ran
    /// `init_db` used to get one; the parallel workers all opened bare
    /// connections that answered a locked cache with an immediate SQLITE_BUSY.
    #[test]
    fn every_connection_waits_out_a_busy_cache() {
        let dir = std::env::temp_dir().join(format!("bgone_oracle_busy_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let oracle = Oracle::new(MakeEnv::new(&dir), dir.join("cache.db"));
        let conn = oracle.open().unwrap();
        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
            .unwrap();
        assert!(
            timeout > 0,
            "a worker connection would not wait for the lock"
        );

        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
