//! The simulated-user stage: oracle self-tests first, then the fixed-seed
//! runs, the determinism check, and the env-gated hunting mode.
//!
//! Seeds: BGONE_SIM_SEED=<n> replays exactly one run of exactly that seed
//! (with BGONE_SIM_ACTIONS steps, default 1000). Unset, the fixed seed set
//! below runs — bounded, deterministic, and part of every `cargo test`.

mod common;
mod sim;

use std::collections::{BTreeMap, BTreeSet, HashSet};

use sim::{
    check_evals, check_group_rules, check_live_set, check_make_conf, check_no_duplicates,
    check_options_file, check_relations, run_sim, OptView, ALL_KINDS,
};

// ============================================================================
// Oracle self-tests: every invariant fed the responses a *broken* system
// would give, asserting it complains. An invariant that never fires is
// indistinguishable from a passing suite — this is the failure mode most
// likely to go unnoticed, so it is tested before the engine ever runs.
// ============================================================================

fn opt(port: &str, name: &str, enabled: bool, group: (&str, &str)) -> OptView {
    OptView {
        port: port.into(),
        name: name.into(),
        enabled,
        group_type: group.0.into(),
        group_name: group.1.into(),
        implies: Vec::new(),
        prevents: Vec::new(),
    }
}

#[test]
fn the_group_oracle_fires_on_broken_groups() {
    // Two SINGLE members set — the c40c913 shape.
    let two_on = vec![
        opt("www/a", "MYSQL", true, ("SINGLE", "BACKEND")),
        opt("www/a", "PGSQL", true, ("SINGLE", "BACKEND")),
    ];
    assert!(check_group_rules(&two_on).is_err(), "must fire on two set");

    // An emptied SINGLE.
    let none_on = vec![
        opt("www/a", "MYSQL", false, ("SINGLE", "BACKEND")),
        opt("www/a", "PGSQL", false, ("SINGLE", "BACKEND")),
    ];
    assert!(
        check_group_rules(&none_on).is_err(),
        "must fire on none set"
    );

    // A lawful SINGLE and a cleared RADIO pass.
    let fine = vec![
        opt("www/a", "MYSQL", true, ("SINGLE", "BACKEND")),
        opt("www/a", "PGSQL", false, ("SINGLE", "BACKEND")),
        opt("www/a", "GNUTLS", false, ("RADIO", "TLS")),
    ];
    assert!(check_group_rules(&fine).is_ok());

    let radio_two = vec![
        opt("www/a", "GNUTLS", true, ("RADIO", "TLS")),
        opt("www/a", "OPENSSL", true, ("RADIO", "TLS")),
    ];
    assert!(check_group_rules(&radio_two).is_err());
}

#[test]
fn the_relations_oracle_fires_on_broken_implications() {
    // A on, implied B off — the dc08d2c shape.
    let mut a = opt("www/a", "NJS", true, ("DEFINE", ""));
    a.implies = vec!["STREAM".into()];
    let violated = vec![a.clone(), opt("www/a", "STREAM", false, ("DEFINE", ""))];
    assert!(
        check_relations(&violated).is_err(),
        "must fire on A-on/B-off"
    );

    let satisfied = vec![a, opt("www/a", "STREAM", true, ("DEFINE", ""))];
    assert!(check_relations(&satisfied).is_ok());

    // Both sides of a PREVENTS on — the 7cd8e4a shape.
    let mut d = opt("www/a", "DEBUG", true, ("DEFINE", ""));
    d.prevents = vec!["STREAM".into()];
    let both_on = vec![d.clone(), opt("www/a", "STREAM", true, ("DEFINE", ""))];
    assert!(check_relations(&both_on).is_err(), "must fire on both-on");

    // ...except when the prevented one is a SINGLE member, which the group
    // must keep — the lawful carve-out.
    let mut p = opt("www/a", "DEBUG", true, ("DEFINE", ""));
    p.prevents = vec!["MYSQL".into()];
    let carved = vec![p, opt("www/a", "MYSQL", true, ("SINGLE", "BACKEND"))];
    assert!(check_relations(&carved).is_ok(), "the carve-out must hold");
}

#[test]
fn the_duplicate_oracle_fires_on_a_repeated_port() {
    let dup = vec!["www/a".to_string(), "www/b".into(), "www/a".into()];
    assert!(check_no_duplicates(&dup, "the list").is_err());
    let fine = vec!["www/a".to_string(), "www/b".into()];
    assert!(check_no_duplicates(&fine, "the list").is_ok());
}

#[test]
fn the_liveness_oracle_fires_on_a_diverging_set() {
    let expected: BTreeSet<String> = ["www/a", "www/b"].iter().map(|s| s.to_string()).collect();
    let missing: BTreeSet<String> = ["www/a"].iter().map(|s| s.to_string()).collect();
    let extra: BTreeSet<String> = ["www/a", "www/b", "www/c"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(check_live_set(&expected, &missing).is_err(), "missing port");
    assert!(check_live_set(&expected, &extra).is_err(), "extra port");
    assert!(check_live_set(&expected, &expected.clone()).is_ok());
}

#[test]
fn the_options_file_oracle_fires_on_incomplete_files() {
    let defined: BTreeSet<String> = ["DOCS", "SSL"].iter().map(|s| s.to_string()).collect();

    // An omitted option is what re-opens poudriere's dialog.
    let missing = "OPTIONS_FILE_SET+=DOCS\n";
    assert!(check_options_file(missing, &defined).is_err());

    // The same option in both lists leaves the framework to pick.
    let both = "OPTIONS_FILE_SET+=DOCS\nOPTIONS_FILE_UNSET+=DOCS\nOPTIONS_FILE_SET+=SSL\n";
    assert!(check_options_file(both, &defined).is_err());

    let alien = "OPTIONS_FILE_SET+=DOCS\nOPTIONS_FILE_UNSET+=SSL\nOPTIONS_FILE_SET+=X11\n";
    assert!(check_options_file(alien, &defined).is_err());

    let fine = "OPTIONS_FILE_SET+=DOCS\nOPTIONS_FILE_UNSET+=SSL\n";
    assert!(check_options_file(fine, &defined).is_ok());
}

#[test]
fn the_make_conf_oracle_fires_on_a_destroyed_file() {
    // The 250aa04 shape: the user's content replaced wholesale.
    let wiped = "# BEGIN bgone\nOPTIONS_SET+=SSL\n# END bgone\n";
    assert!(check_make_conf(wiped, "CFLAGS+=-O2").is_err());

    let doubled = "CFLAGS+=-O2\n# BEGIN bgone\n# END bgone\n# BEGIN bgone\n# END bgone\n";
    assert!(check_make_conf(doubled, "CFLAGS+=-O2").is_err());

    let fine = "CFLAGS+=-O2\n# BEGIN bgone\nOPTIONS_SET+=SSL\n# END bgone\n";
    assert!(check_make_conf(fine, "CFLAGS+=-O2").is_ok());
}

#[test]
fn the_eval_oracle_fires_on_runaways_and_broken_memos() {
    let visited: HashSet<String> = ["www/a|0|".to_string()].into_iter().collect();
    let none = BTreeSet::new();

    // Over the cap: the runaway-loop analogue.
    let burst: Vec<String> = (0..10).map(|i| format!("www/p{i}|0|")).collect();
    assert!(check_evals(&burst, &visited, &none, 3, "t").is_err());

    // A pair evaluated again while the memo should hold it.
    let revisit = vec!["www/a|0|".to_string()];
    assert!(check_evals(&revisit, &visited, &none, 10, "t").is_err());

    // ...unless its port is legitimately stale.
    let exempt: BTreeSet<String> = ["www/a".to_string()].into_iter().collect();
    assert!(check_evals(&revisit, &visited, &exempt, 10, "t").is_ok());

    let fresh = vec!["www/b|0|".to_string()];
    assert!(check_evals(&fresh, &visited, &none, 10, "t").is_ok());
}

// ============================================================================
// The engine itself: fixed seeds, gating, determinism, hunting mode.
// ============================================================================

/// A toggle left unsettled must not trip the liveness gate — the graph
/// legitimately lags the spec until a resettle. This is the negative
/// self-test for the gating, run against a tape of nothing but toggles.
#[test]
fn an_unsettled_run_does_not_false_fire() {
    use sim::{run_tape, Kind, Slot};
    let tape: Vec<Slot> = (0..12u32)
        .map(|i| Slot {
            kind: Kind::Toggle,
            args: [i.wrapping_mul(2654435761), i, i, i],
        })
        .collect();
    if let Err(failure) = run_tape(41, &tape) {
        panic!("{failure}");
    }
}

/// The always-on stage: two fixed seeds, every action kind exercised at
/// least once across them, wall-clock reported so the cost stays a decision.
#[test]
fn fixed_seed_runs_stay_green() {
    let started = std::time::Instant::now();
    let mut fired: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut skipped_total = 0usize;

    for seed in [1u32, 2] {
        println!("simulated-user run: seed {seed}");
        match run_sim(seed, 225) {
            Ok(report) => {
                for (kind, (f, s)) in &report.histogram {
                    *fired.entry(kind).or_default() += f;
                    skipped_total += s;
                }
                println!(
                    "  seed {}: {} executed, {} skipped",
                    report.seed, report.executed, report.skipped
                );
            }
            Err(failure) => panic!("{failure}"),
        }
    }

    // A kind that never fires is silently absent coverage; say so loudly.
    let mut never: Vec<&str> = ALL_KINDS
        .iter()
        .map(|&k| sim::kind_label(k))
        .filter(|name| fired.get(name).copied().unwrap_or(0) == 0)
        .collect();
    never.sort();
    assert!(
        never.is_empty(),
        "action kinds never fired across the seed set: {never:?}"
    );

    println!(
        "simulated-user stage: {:.1}s, {skipped_total} skipped slots",
        started.elapsed().as_secs_f64()
    );
}

/// The same seed must replay identically — the reproducibility contract every
/// randomised finding depends on.
#[test]
fn the_same_seed_replays_identically() {
    let first = run_sim(3, 120).unwrap_or_else(|f| panic!("{f}"));
    let second = run_sim(3, 120).unwrap_or_else(|f| panic!("{f}"));
    assert_eq!(
        first.trace, second.trace,
        "a seed that replays differently reports bugs nobody can chase"
    );
}

/// Hunting mode: BGONE_SIM_SEED means one run of exactly that seed, nothing
/// else — replaying a failure must never silently turn into something else.
#[test]
fn hunting_mode_when_requested() {
    let Some(seed) = std::env::var("BGONE_SIM_SEED")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };
    let actions = std::env::var("BGONE_SIM_ACTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1000);

    println!("hunting: seed {seed}, {actions} actions");
    let started = std::time::Instant::now();
    match run_sim(seed, actions) {
        Ok(report) => println!(
            "seed {seed}: {} executed, {} skipped, {:.1}s",
            report.executed,
            report.skipped,
            started.elapsed().as_secs_f64()
        ),
        Err(failure) => panic!("{failure}"),
    }
}
