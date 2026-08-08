//! Differential smoke tests against a real FreeBSD ports tree.
//!
//! Every other suite validates the resolver against a stub `make` that
//! re-implements `bsd.options.mk`'s behaviour — which means a drift in the
//! stub's model of the framework passes every test while the tool is wrong on
//! FreeBSD. These tests close that loop: they run only on FreeBSD, only when
//! `BGONE_PORTS_TREE` names a tree (usually `/usr/ports`), and ask the real
//! framework to disagree.
//!
//! Not run in CI — there is no FreeBSD runner there; the release binaries are
//! cross-compiled. On a FreeBSD machine:
//!
//! ```sh
//! BGONE_PORTS_TREE=/usr/ports cargo test --test freebsd_tree_tests
//! ```
#![cfg(target_os = "freebsd")]

use bgone::oracle::{Options, Oracle};
use bgone::resolve::MakeEnv;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A small port that exists in every tree and defines options.
const ORIGIN: &str = "ports-mgmt/pkg";

fn tree() -> Option<PathBuf> {
    std::env::var_os("BGONE_PORTS_TREE").map(PathBuf::from)
}

/// Asks make directly, neutralising the host's configuration the same way the
/// resolver does, so both sides answer the same question.
fn make_v(tree: &Path, origin: &str, var: &str) -> String {
    let out = Command::new("make")
        .arg("-C")
        .arg(tree.join(origin))
        .arg("-V")
        .arg(var)
        .arg(format!("PORTSDIR={}", tree.display()))
        .arg("__MAKE_CONF=/dev/null")
        .arg("PORT_DBDIR=/nonexistent")
        .arg("OPTIONS_SET=")
        .arg("OPTIONS_UNSET=")
        .output()
        .expect("running make");
    assert!(out.status.success(), "make -V {var} failed for {origin}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The resolver's facts must agree with what the framework itself reports.
/// This is the whole trust argument: the stub suite proves bgone matches the
/// stub, and this proves the resolver matches the framework.
#[test]
fn facts_agree_with_the_real_framework() {
    let Some(tree) = tree() else {
        eprintln!("BGONE_PORTS_TREE not set; skipping the differential check");
        return;
    };

    let tmp = std::env::temp_dir().join(format!("bgone_fbsd_diff_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let oracle = Oracle::new(MakeEnv::new(&tree), tmp.join("cache.db"));

    let facts = oracle
        .facts(ORIGIN, &Options::AsShipped)
        .expect("the resolver must be able to evaluate a real port");

    assert_eq!(
        facts.pkgname,
        make_v(&tree, ORIGIN, "PKGNAME"),
        "PKGNAME must match the framework's"
    );

    let mut expected: Vec<String> = make_v(&tree, ORIGIN, "COMPLETE_OPTIONS_LIST")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    expected.sort();
    expected.dedup();
    let mut got: Vec<String> = facts.options.iter().map(|o| o.name.clone()).collect();
    got.sort();
    assert_eq!(
        got, expected,
        "the option list must match the framework's COMPLETE_OPTIONS_LIST"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
