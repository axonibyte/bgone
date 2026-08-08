//! Simulated-user testing: seeded random *sequences* of user actions driven
//! against the real resolver stack, with a partial shadow model and
//! history-independent invariants checked after every step.
//!
//! This is stateful property-based testing (Hypothesis's RuleBasedStateMachine,
//! Erlang QuickCheck's eqc_statem) in the Jepsen vocabulary: a generator picks
//! weighted actions whose applicability widens as state accumulates, a nemesis
//! interleaves adversarial ones (external edits, vanished ports, a dropped
//! cache), a checker diffs the system against the model and asserts invariants
//! that need no prediction, and the history is a full trace printed on
//! failure. The class of defect it exists for is the one scripted tests
//! systematically miss: bugs whose reachability needs a *prefix* of prior
//! actions — most of the defects fixed in this repo's last bug series were of
//! exactly that shape, and each invariant below names the commit it traces to.
//!
//! Discipline: the seed is printed on every run and replayable exactly via
//! BGONE_SIM_SEED; failures print the whole trace; the action tape is fully
//! pre-drawn from the seed so a shrinker can cut it without reshuffling later
//! draws; every replay runs in a fresh temp world, so replays are clean.
//!
//! Known generation gaps, stated rather than silent: the fixture cannot emit
//! flavoured dependency entries; implies/prevents edges are generated between
//! plain (DEFINE) options only — an implication into a SINGLE member whose
//! sibling later displaces it is a real shape the engine does not yet model;
//! group membership agreement across sessions is not asserted (the product
//! does not re-align members at load).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use bgone::config::{save_groups, Config, Groups};
use bgone::exporter;
use bgone::graph::{DependencyGraph, NodeId, Provenance};
use bgone::reader::SystemOptions;

use crate::common::{self, TempDir};

// ---------------------------------------------------------------- PRNG

/// xorshift32: tiny, seedable, and deterministic across platforms — all this
/// needs. Zero is mapped away because xorshift fixes it.
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self(if seed == 0 { 0x9e37_79b9 } else { seed })
    }

    pub fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }

    pub fn chance(&mut self, pct: u32) -> bool {
        self.next() % 100 < pct
    }
}

// ---------------------------------------------------------------- world spec

/// The engine's own description of the tree it generated. Everything the
/// liveness oracle knows comes from here, never from the resolver under test.
#[derive(Clone)]
pub struct SpecOption {
    pub name: String,
    pub default_on: bool,
    pub group_type: String,
    pub group_name: String,
    pub implies: Vec<String>,
    pub prevents: Vec<String>,
    pub deps_on: Vec<String>,
    pub deps_off: Vec<String>,
    pub hidden: Vec<String>,
}

#[derive(Clone, Default)]
pub struct SpecPort {
    pub origin: String,
    pub options: Vec<SpecOption>,
    pub deps: Vec<String>,
}

pub struct World {
    pub ports: Vec<SpecPort>,
    pub roots: Vec<String>,
    /// The one port the nemesis may make unreadable; never a root.
    pub unreadable_candidate: Option<String>,
}

impl World {
    pub fn port(&self, origin: &str) -> Option<&SpecPort> {
        self.ports.iter().find(|p| p.origin == origin)
    }

    /// Whether an option is an endpoint of any implies/prevents edge on its
    /// port. External state (pre-seeds, hand edits, group adoption) is kept
    /// away from entangled options: the product applies relations on toggles,
    /// not on loaded or adopted state, so external writes there can create
    /// combinations no session action could — a separate product edge,
    /// recorded as a finding rather than fuzzed into every run.
    pub fn entangled(&self, origin: &str, name: &str) -> bool {
        let Some(port) = self.port(origin) else {
            return false;
        };
        port.options.iter().any(|o| {
            (o.name == name && (!o.implies.is_empty() || !o.prevents.is_empty()))
                || o.implies.iter().any(|n| n == name)
                || o.prevents.iter().any(|n| n == name)
        })
    }

    fn port_mut(&mut self, origin: &str) -> &mut SpecPort {
        let i = self
            .ports
            .iter()
            .position(|p| p.origin == origin)
            .expect("spec port exists");
        &mut self.ports[i]
    }
}

const CATEGORIES: [&str; 4] = ["www", "databases", "devel", "security"];
const OPTION_POOL: [&str; 10] = [
    "DOCS", "NLS", "X11", "SSL", "DEBUG", "STREAM", "NJS", "CACHE", "ZSTD", "LZ4",
];

pub fn gen_world(rng: &mut Rng) -> World {
    let n = 12 + rng.below(5);
    let mut ports: Vec<SpecPort> = (0..n)
        .map(|i| SpecPort {
            origin: format!("{}/p{:02}", CATEGORIES[i % CATEGORIES.len()], i),
            ..Default::default()
        })
        .collect();

    // Plain options; names collide across ports on purpose, which is what
    // makes group sync and make.conf unanimity mean anything.
    for port in ports.iter_mut() {
        let count = rng.below(4);
        let mut names: Vec<&str> = OPTION_POOL.to_vec();
        for _ in 0..count {
            let name = names.remove(rng.below(names.len()));
            port.options.push(SpecOption {
                name: name.to_string(),
                default_on: rng.chance(40),
                group_type: "DEFINE".into(),
                group_name: String::new(),
                implies: Vec::new(),
                prevents: Vec::new(),
                deps_on: Vec::new(),
                deps_off: Vec::new(),
                hidden: Vec::new(),
            });
        }
    }

    // Two SINGLE groups with overlapping-but-not-identical membership, one
    // RADIO. Exactly one SINGLE member defaults on; a RADIO may have none.
    let singles: [&[&str]; 2] = [&["MYSQL", "PGSQL", "SQLITE"], &["PGSQL", "SQLITE"]];
    for (which, port_idx) in [(0usize, 0usize), (1, 3)] {
        for (i, name) in singles[which].iter().enumerate() {
            ports[port_idx].options.push(SpecOption {
                name: name.to_string(),
                default_on: i == 0,
                group_type: "SINGLE".into(),
                group_name: "BACKEND".into(),
                implies: Vec::new(),
                prevents: Vec::new(),
                deps_on: Vec::new(),
                deps_off: Vec::new(),
                hidden: Vec::new(),
            });
        }
    }
    for (i, name) in ["GNUTLS", "OPENSSL"].iter().enumerate() {
        let on = i == 0 && rng.chance(50);
        ports[1].options.push(SpecOption {
            name: name.to_string(),
            default_on: on,
            group_type: "RADIO".into(),
            group_name: "TLSLIB".into(),
            implies: Vec::new(),
            prevents: Vec::new(),
            deps_on: Vec::new(),
            deps_off: Vec::new(),
            hidden: Vec::new(),
        });
    }

    // Unconditional deps, forward-only, with two forced diamonds so shared
    // dependencies actually occur.
    for i in 2..n {
        if rng.chance(50) {
            let from = rng.below(i);
            let to = ports[i].origin.clone();
            ports[from].deps.push(to);
        }
    }
    let shared = ports[n - 1].origin.clone();
    ports[0].deps.push(shared.clone());
    ports[1].deps.push(shared);

    // Option-conditional deps, both polarities, plus hidden (procedural) ones.
    let pick_opt = |rng: &mut Rng, ports: &[SpecPort]| -> Option<(usize, usize)> {
        for _ in 0..20 {
            let pi = rng.below(ports.len());
            if !ports[pi].options.is_empty() {
                let oi = rng.below(ports[pi].options.len());
                if ports[pi].options[oi].group_type == "DEFINE" {
                    return Some((pi, oi));
                }
            }
        }
        None
    };
    for kind in 0..5 {
        if let Some((pi, oi)) = pick_opt(rng, &ports) {
            let target = ports[rng.below(n)].origin.clone();
            if target == ports[pi].origin {
                continue;
            }
            let opt = &mut ports[pi].options[oi];
            match kind {
                0 | 1 => opt.deps_on.push(target),
                2 => opt.deps_off.push(target),
                _ => opt.hidden.push(target),
            }
        }
    }

    // implies/prevents between DEFINE options of one port, endpoint pools
    // disjoint so the two relations cannot contradict each other — a spec
    // that says A implies B while something prevents B has no satisfiable
    // answer for the invariants to check.
    let mut relation_endpoints: HashSet<(String, String)> = HashSet::new();
    for relation in 0..4 {
        for _ in 0..20 {
            let pi = rng.below(n);
            let defines: Vec<usize> = ports[pi]
                .options
                .iter()
                .enumerate()
                .filter(|(_, o)| o.group_type == "DEFINE")
                .map(|(i, _)| i)
                .collect();
            if defines.len() < 2 {
                continue;
            }
            let a = defines[rng.below(defines.len())];
            let b = defines[rng.below(defines.len())];
            if a == b {
                continue;
            }
            let origin = ports[pi].origin.clone();
            let (na, nb) = (
                ports[pi].options[a].name.clone(),
                ports[pi].options[b].name.clone(),
            );
            if relation_endpoints.contains(&(origin.clone(), na.clone()))
                || relation_endpoints.contains(&(origin.clone(), nb.clone()))
            {
                continue;
            }
            relation_endpoints.insert((origin.clone(), na));
            relation_endpoints.insert((origin, nb));
            let name_b = ports[pi].options[b].name.clone();
            if relation < 3 {
                ports[pi].options[a].implies.push(name_b);
            } else {
                ports[pi].options[a].prevents.push(name_b);
            }
            break;
        }
    }

    // The shipped defaults have to satisfy the relations they declare: the
    // product enforces implications on *toggles*, not on loaded state, so a
    // world whose defaults already violate closure would fire the invariant
    // before the first action. (That load-time non-enforcement is a real,
    // observed product edge — recorded as a finding, not silently absorbed.)
    for _ in 0..3 {
        for port in ports.iter_mut() {
            for oi in 0..port.options.len() {
                if port.options[oi].default_on {
                    for name in port.options[oi].implies.clone() {
                        if let Some(t) = port.options.iter_mut().find(|o| o.name == name) {
                            t.default_on = true;
                        }
                    }
                    for name in port.options[oi].prevents.clone() {
                        if let Some(t) = port.options.iter_mut().find(|o| o.name == name) {
                            t.default_on = false;
                        }
                    }
                }
            }
        }
    }

    let roots: Vec<String> = ports.iter().take(2).map(|p| p.origin.clone()).collect();
    // A dependency target, never a root, so a failed evaluation is always the
    // recorded-and-carried-on path rather than the fatal all-roots-failed one.
    let unreadable_candidate = ports
        .iter()
        .skip(4)
        .map(|p| &p.origin)
        .find(|o| !roots.contains(o))
        .cloned();

    World {
        ports,
        roots,
        unreadable_candidate,
    }
}

fn build_tree(tag: &str, world: &World) -> common::Tree {
    let mut tree = common::Tree::new(tag);
    for port in &world.ports {
        tree.add_port(&port.origin);
        for o in &port.options {
            tree.add_option(
                &port.origin,
                &o.name,
                o.default_on,
                "",
                &o.group_type,
                &o.group_name,
            );
            for t in &o.deps_on {
                tree.add_option_dep_with(&port.origin, &o.name, t, "LIB", "ON");
            }
            for t in &o.deps_off {
                tree.add_option_dep_with(&port.origin, &o.name, t, "LIB", "OFF");
            }
            for t in &o.hidden {
                tree.add_hidden_dep(&port.origin, &o.name, t);
            }
            for i in &o.implies {
                tree.add_implies(&port.origin, &o.name, i);
            }
            for p in &o.prevents {
                tree.add_prevents(&port.origin, &o.name, p);
            }
        }
        for t in &port.deps {
            tree.add_port_dep(&port.origin, t);
        }
    }
    tree
}

// ---------------------------------------------------------------- invariants

/// One option as the invariants see it — a deliberately thin view, buildable
/// from the graph or by hand for the oracle self-tests.
#[derive(Clone)]
pub struct OptView {
    pub port: String,
    pub name: String,
    pub enabled: bool,
    pub group_type: String,
    pub group_name: String,
    pub implies: Vec<String>,
    pub prevents: Vec<String>,
}

/// SINGLE holds exactly one set member, RADIO at most one.
/// Traces to c40c913: implications used to write past the group rules.
pub fn check_group_rules(opts: &[OptView]) -> Result<(), String> {
    let mut counts: BTreeMap<(String, String, String), usize> = BTreeMap::new();
    for o in opts {
        if o.group_type == "SINGLE" || o.group_type == "RADIO" {
            let key = (o.port.clone(), o.group_type.clone(), o.group_name.clone());
            *counts.entry(key).or_default() += usize::from(o.enabled);
        }
    }
    for ((port, ty, group), set) in counts {
        if ty == "SINGLE" && set != 1 {
            return Err(format!("SINGLE {group} on {port} holds {set} set members"));
        }
        if ty == "RADIO" && set > 1 {
            return Err(format!("RADIO {group} on {port} holds {set} set members"));
        }
    }
    Ok(())
}

/// Enabled options honour their IMPLIES and PREVENTS declarations — the
/// combinations bsd.options.mk would override or reject. The one lawful
/// exception: a prevented option that is a SINGLE member stays set, because
/// the group must keep one (`prevents_cannot_empty_a_single`).
/// Traces to dc08d2c / 7cd8e4a.
pub fn check_relations(opts: &[OptView]) -> Result<(), String> {
    let by_key: HashMap<(&str, &str), &OptView> = opts
        .iter()
        .map(|o| ((o.port.as_str(), o.name.as_str()), o))
        .collect();
    for o in opts.iter().filter(|o| o.enabled) {
        for name in &o.implies {
            if let Some(t) = by_key.get(&(o.port.as_str(), name.as_str())) {
                if !t.enabled {
                    return Err(format!(
                        "{}: {} is on but implied {} is off",
                        o.port, o.name, name
                    ));
                }
            }
        }
        for name in &o.prevents {
            if let Some(t) = by_key.get(&(o.port.as_str(), name.as_str())) {
                if t.enabled && t.group_type != "SINGLE" {
                    return Err(format!(
                        "{}: {} is on alongside prevented {}",
                        o.port, o.name, name
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Every port appears once — the flat-list contract the README leads with.
pub fn check_no_duplicates(origins: &[String], what: &str) -> Result<(), String> {
    let mut seen = HashSet::new();
    for o in origins {
        if !seen.insert(o) {
            return Err(format!("{o} appears twice in {what}"));
        }
    }
    Ok(())
}

/// The build the resolver computed matches the one the spec computes.
/// Traces to 2b619ab: a port arriving mid-session used to get the wrong
/// dependency set, which is exactly a wrong live set.
pub fn check_live_set(
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
) -> Result<(), String> {
    if expected != actual {
        let missing: Vec<&String> = expected.difference(actual).collect();
        let extra: Vec<&String> = actual.difference(expected).collect();
        return Err(format!(
            "live set diverges from spec: missing {missing:?}, extra {extra:?}"
        ));
    }
    Ok(())
}

/// Every option the port defines appears exactly once across
/// OPTIONS_FILE_SET/UNSET — an omission is what re-opens poudriere's dialog.
pub fn check_options_file(content: &str, defined: &BTreeSet<String>) -> Result<(), String> {
    let mut listed: BTreeMap<String, usize> = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        for prefix in ["OPTIONS_FILE_SET+=", "OPTIONS_FILE_UNSET+="] {
            if let Some(name) = line.strip_prefix(prefix) {
                *listed.entry(name.trim().to_string()).or_default() += 1;
            }
        }
    }
    for name in defined {
        match listed.get(name) {
            Some(1) => {}
            Some(n) => return Err(format!("{name} listed {n} times")),
            None => return Err(format!("{name} missing from the options file")),
        }
    }
    for name in listed.keys() {
        if !defined.contains(name) {
            return Err(format!("{name} listed but not defined"));
        }
    }
    Ok(())
}

/// make.conf holds exactly one managed block and the user's own content
/// survives. Traces to 250aa04: the export used to replace the whole file.
pub fn check_make_conf(content: &str, sentinel: &str) -> Result<(), String> {
    let begins = content.matches("# BEGIN bgone").count();
    let ends = content.matches("# END bgone").count();
    if begins != 1 || ends != 1 {
        return Err(format!("expected one managed block, found {begins}/{ends}"));
    }
    if !content.contains(sentinel) {
        return Err(format!("user content {sentinel:?} was destroyed"));
    }
    Ok(())
}

/// The request-rate oracle: evaluations this action caused, from the stub's
/// log. Bounded, and no (port, option-set) pair is evaluated twice while the
/// memo should still hold it — a re-miss is either a broken cache or a loop.
/// Counts misses, not asks: a runaway that loops on a *cached* question is
/// invisible here, which is a stated gap, not a covered case.
pub fn check_evals(
    new_pairs: &[String],
    visited: &HashSet<String>,
    exempt_origins: &BTreeSet<String>,
    cap: usize,
    action: &str,
) -> Result<(), String> {
    if new_pairs.len() > cap {
        return Err(format!(
            "{action}: {} evaluations, cap {cap}: {new_pairs:?}",
            new_pairs.len()
        ));
    }
    for pair in new_pairs {
        let origin = pair.split('|').next().unwrap_or("");
        if visited.contains(pair) && !exempt_origins.iter().any(|o| o == origin) {
            return Err(format!(
                "{action}: {pair} re-evaluated while the memo should hold it"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- tape

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Toggle,
    DoubleToggle,
    Resettle,
    View,
    GroupCreate,
    GroupRoundTrip,
    Export,
    ExportDryRun,
    Reload,
    NewSession,
    ExternalEditOptions,
    ExternalGlobals,
    PreSeed,
    TreeMutate,
    MakeUnreadable,
    RestoreUnreadable,
    DropCache,
}

pub const ALL_KINDS: [Kind; 17] = [
    Kind::Toggle,
    Kind::DoubleToggle,
    Kind::Resettle,
    Kind::View,
    Kind::GroupCreate,
    Kind::GroupRoundTrip,
    Kind::Export,
    Kind::ExportDryRun,
    Kind::Reload,
    Kind::NewSession,
    Kind::ExternalEditOptions,
    Kind::ExternalGlobals,
    Kind::PreSeed,
    Kind::TreeMutate,
    Kind::MakeUnreadable,
    Kind::RestoreUnreadable,
    Kind::DropCache,
];

fn weight(kind: Kind) -> u32 {
    match kind {
        Kind::Toggle => 30,
        Kind::DoubleToggle => 5,
        Kind::Resettle => 15,
        Kind::View => 8,
        Kind::GroupCreate => 3,
        Kind::GroupRoundTrip => 2,
        Kind::Export => 8,
        Kind::ExportDryRun => 2,
        Kind::Reload => 4,
        Kind::NewSession => 5,
        Kind::ExternalEditOptions => 3,
        Kind::ExternalGlobals => 2,
        Kind::PreSeed => 3,
        Kind::TreeMutate => 2,
        Kind::MakeUnreadable => 2,
        Kind::RestoreUnreadable => 4,
        Kind::DropCache => 2,
    }
}

/// One pre-drawn step: the kind, plus a fixed block of randoms its arguments
/// are resolved from at execution time. Pre-drawing the whole tape is what
/// lets a shrinker cut steps without reshuffling every later draw.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub kind: Kind,
    pub args: [u32; 4],
}

pub fn draw_tape(seed: u32, n: usize) -> Vec<Slot> {
    // A distinct stream from the world's, so tape length never changes the
    // world the seed names.
    let mut rng = Rng::new(seed ^ 0xa5a5_a5a5);
    let total: u32 = ALL_KINDS.iter().map(|&k| weight(k)).sum();
    (0..n)
        .map(|_| {
            let mut pick = rng.next() % total;
            let mut kind = Kind::Toggle;
            for &k in &ALL_KINDS {
                if pick < weight(k) {
                    kind = k;
                    break;
                }
                pick -= weight(k);
            }
            Slot {
                kind,
                args: [rng.next(), rng.next(), rng.next(), rng.next()],
            }
        })
        .collect()
}

// ---------------------------------------------------------------- engine

pub struct Step {
    pub ordinal: usize,
    pub action: String,
    pub outcome: String,
}

pub struct Failure {
    pub seed: u32,
    pub at: usize,
    pub message: String,
    pub trace: Vec<Step>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "simulated-user run failed at step {} (seed {}, replay with BGONE_SIM_SEED={}):",
            self.at, self.seed, self.seed
        )?;
        writeln!(f, "  {}", self.message)?;
        writeln!(f, "trace:")?;
        for s in &self.trace {
            writeln!(f, "  #{:04} {} -> {}", s.ordinal, s.action, s.outcome)?;
        }
        Ok(())
    }
}

pub struct Report {
    pub seed: u32,
    pub executed: usize,
    pub skipped: usize,
    pub histogram: BTreeMap<&'static str, (usize, usize)>,
    /// The full action trace, for the determinism check: same seed, same
    /// trace, or the reproducibility contract is broken.
    pub trace: Vec<String>,
}

pub fn kind_label(kind: Kind) -> &'static str {
    kind_name(kind)
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Toggle => "toggle",
        Kind::DoubleToggle => "double-toggle",
        Kind::Resettle => "resettle",
        Kind::View => "view",
        Kind::GroupCreate => "group-create",
        Kind::GroupRoundTrip => "group-roundtrip",
        Kind::Export => "export",
        Kind::ExportDryRun => "export-dry-run",
        Kind::Reload => "reload",
        Kind::NewSession => "new-session",
        Kind::ExternalEditOptions => "external-edit-options",
        Kind::ExternalGlobals => "external-globals",
        Kind::PreSeed => "pre-seed",
        Kind::TreeMutate => "tree-mutate",
        Kind::MakeUnreadable => "make-unreadable",
        Kind::RestoreUnreadable => "restore-unreadable",
        Kind::DropCache => "drop-cache",
    }
}

struct Model {
    /// Option states read back from the graph after each action — absorbed,
    /// not predicted, so the model never becomes a second implementation of
    /// the implication rules.
    states: HashMap<String, HashMap<String, bool>>,
    /// Ports toggled since the last resettle; while non-empty the graph
    /// legitimately lags the spec, so liveness equality is gated on it.
    pending_touched: BTreeSet<String>,
    /// Ports whose .port the nemesis removed. The graph legitimately serves
    /// their last-known edges, so liveness is suspended while any exist.
    stale: BTreeSet<String>,
    /// The tree changed under the running session; only a new session sees
    /// the new spec, so liveness is suspended until one starts.
    world_stale: bool,
    /// Per-port option files on disk, as this engine believes them: bgone
    /// exports, external corrections, and pre-seeded files alike.
    exported: HashMap<String, HashMap<String, bool>>,
    /// (origin, option) pairs whose on-disk value came from an external edit;
    /// an export supersedes them, which is logged rather than asserted.
    external_edits: HashSet<(String, String)>,
    /// Globals the external actor wrote above the managed block.
    external_globals: BTreeMap<String, bool>,
    /// Globals the managed block held after the last export (unanimity).
    block_globals: BTreeMap<String, bool>,
    /// (origin|has_override|override) pairs the stub has answered — the memo
    /// should hold them until something legitimately ages the key.
    visited: HashSet<String>,
    groups: Groups,
}

pub fn run_sim(seed: u32, actions: usize) -> Result<Report, Box<Failure>> {
    let tape = draw_tape(seed, actions);
    run_tape(seed, &tape)
}

pub fn run_tape(seed: u32, tape: &[Slot]) -> Result<Report, Box<Failure>> {
    Engine::new(seed).run(tape)
}

const SENTINEL: &str = "CFLAGS+=-O2 -pipe # sim-user-content";

struct Engine {
    seed: u32,
    world: World,
    tree: common::Tree,
    io: TempDir,
    graph: DependencyGraph,
    session_opts: SystemOptions,
    model: Model,
    trace: Vec<Step>,
    clock_bump: u64,
    log_seen: usize,
    mutations: usize,
    histogram: BTreeMap<&'static str, (usize, usize)>,
}

impl Engine {
    fn new(seed: u32) -> Self {
        let mut rng = Rng::new(seed);
        let world = gen_world(&mut rng);
        let mut tree = build_tree(&format!("sim_{seed}"), &world);
        let io = TempDir::new(&format!("sim_io_{seed}"));
        fs::create_dir_all(io.join("options")).unwrap();
        fs::write(io.join("make.conf"), format!("{SENTINEL}\n")).unwrap();

        let session_opts = SystemOptions::default();
        let mut graph = tree
            .build(&world.roots.clone(), &session_opts, false)
            .expect("the generated world must build");
        graph.expand_all();

        let mut engine = Self {
            seed,
            world,
            tree,
            io,
            graph,
            session_opts,
            model: Model {
                states: HashMap::new(),
                pending_touched: BTreeSet::new(),
                stale: BTreeSet::new(),
                world_stale: false,
                exported: HashMap::new(),
                external_edits: HashSet::new(),
                external_globals: BTreeMap::new(),
                block_globals: BTreeMap::new(),
                visited: HashSet::new(),
                groups: Groups::new(),
            },
            trace: Vec::new(),
            clock_bump: 0,
            log_seen: 0,
            mutations: 0,
            histogram: BTreeMap::new(),
        };
        for &k in &ALL_KINDS {
            engine.histogram.insert(kind_name(k), (0, 0));
        }
        engine.absorb();
        // The initial cold build is not this run's to judge: mark its
        // evaluations as visited without bounding them.
        let initial = engine.read_new_log_pairs();
        engine.model.visited.extend(initial);
        engine
    }

    fn options_dir(&self) -> PathBuf {
        self.io.join("options")
    }

    fn make_conf(&self) -> PathBuf {
        self.io.join("make.conf")
    }

    // ---------------------------------------------------------- model help

    fn absorb(&mut self) {
        self.model.states.clear();
        for o in self.graph.real_options() {
            self.model
                .states
                .entry(o.port_origin.clone())
                .or_default()
                .insert(o.name.clone(), o.enabled);
        }
    }

    /// The reader's documented precedence, restated: per-port file, then the
    /// effective global, then the spec default. The managed block sits below
    /// the user's globals in the file, so its assignment wins collisions.
    fn expected_from_disk(&self, origin: &str, option: &str, default_on: bool) -> bool {
        if let Some(states) = self.model.exported.get(origin) {
            if let Some(&v) = states.get(option) {
                return v;
            }
        }
        if let Some(&v) = self.model.block_globals.get(option) {
            return v;
        }
        if let Some(&v) = self.model.external_globals.get(option) {
            return v;
        }
        default_on
    }

    fn spec_state(&self, origin: &str, option: &SpecOption) -> bool {
        if let Some(states) = self.model.states.get(origin) {
            if let Some(&v) = states.get(&option.name) {
                return v;
            }
        }
        self.expected_from_disk(origin, &option.name, option.default_on)
    }

    fn spec_reachable(&self, roots: &[String]) -> BTreeSet<String> {
        let mut live: BTreeSet<String> = BTreeSet::new();
        let mut queue: Vec<String> = roots.to_vec();
        while let Some(origin) = queue.pop() {
            if !live.insert(origin.clone()) {
                continue;
            }
            let Some(port) = self.world.port(&origin) else {
                continue;
            };
            let push = |t: &String, queue: &mut Vec<String>| queue.push(t.clone());
            for t in &port.deps {
                push(t, &mut queue);
            }
            for o in &port.options {
                if self.spec_state(&origin, o) {
                    for t in o.deps_on.iter().chain(o.hidden.iter()) {
                        push(t, &mut queue);
                    }
                } else {
                    for t in &o.deps_off {
                        push(t, &mut queue);
                    }
                }
            }
        }
        live
    }

    fn views(&self) -> Vec<OptView> {
        self.graph
            .real_options()
            .map(|o| OptView {
                port: o.port_origin.clone(),
                name: o.name.clone(),
                enabled: o.enabled,
                group_type: o.group_type.clone(),
                group_name: o.group_name.clone(),
                implies: o.implies.clone(),
                prevents: o.prevents.clone(),
            })
            .collect()
    }

    fn read_new_log_pairs(&mut self) -> Vec<String> {
        let content = fs::read_to_string(self.tree.stub_log()).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let new = lines[self.log_seen.min(lines.len())..]
            .iter()
            .map(|line| {
                let mut parts = line.splitn(3, '|');
                let dir = parts.next().unwrap_or("");
                let has = parts.next().unwrap_or("");
                let over = parts.next().unwrap_or("");
                let origin: Vec<&str> = dir.rsplit('/').take(2).collect();
                format!(
                    "{}/{}|{has}|{over}",
                    origin.get(1).unwrap_or(&""),
                    origin.first().unwrap_or(&"")
                )
            })
            .collect();
        self.log_seen = lines.len();
        new
    }

    /// Records evaluated pairs as memoised — except those of stale ports,
    /// whose evaluations *failed* and were never cached: asking them again
    /// later is a legitimate miss, not a broken memo.
    fn note_visited(&mut self, pairs: Vec<String>) {
        for pair in pairs {
            let origin = pair.split('|').next().unwrap_or("").to_string();
            if !self.model.stale.contains(&origin) {
                self.model.visited.insert(pair);
            }
        }
    }

    fn bump_makefile(&mut self, origin: &str) {
        self.clock_bump += 120;
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(self.clock_bump);
        let makefile = self.tree.root().join(origin).join("Makefile");
        if let Ok(handle) = fs::File::options().write(true).open(&makefile) {
            let _ = handle.set_times(fs::FileTimes::new().set_modified(later));
        }
    }

    // ---------------------------------------------------------- checks

    fn check_invariants(&self, settled_ok: bool) -> Result<(), String> {
        let views = self.views();
        check_group_rules(&views)?;
        check_relations(&views)?;
        let ports: Vec<String> = self.graph.ports.iter().map(|p| p.origin.clone()).collect();
        check_no_duplicates(&ports, "the port list")?;
        let rows: Vec<String> = self
            .graph
            .visible_rows
            .iter()
            .filter_map(|r| match r.node_id {
                NodeId::Port(id) => Some(self.graph.ports[id].origin.clone()),
                _ => None,
            })
            .collect();
        check_no_duplicates(&rows, "the visible rows")?;

        for p in self.graph.requested_ports() {
            if p.provenance != Provenance::Requested {
                return Err(format!("{} lost its requested provenance", p.origin));
            }
        }

        // A port the graph knows it could not read must never be hidden as
        // optionless — its options are unknown, not absent. (A port whose
        // .port vanished *after* a successful evaluation still legitimately
        // shows, and hides, its last-known options.)
        if self.graph.hide_optionless && self.graph.search_query.is_empty() {
            for origin in self.graph.unevaluated_ports() {
                if self
                    .graph
                    .visible_rows
                    .iter()
                    .all(|r| !matches!(&r.kind, bgone::graph::RowKind::Port { origin: o, .. } if o == origin))
                {
                    return Err(format!("unreadable {origin} hidden by hide-optionless"));
                }
            }
        }

        if settled_ok
            && self.model.pending_touched.is_empty()
            && self.model.stale.is_empty()
            && !self.model.world_stale
        {
            let expected = self.spec_reachable(&self.current_roots());
            let actual: BTreeSet<String> = self
                .world
                .ports
                .iter()
                .map(|p| p.origin.clone())
                .filter(|o| self.graph.is_live(o))
                .collect();
            check_live_set(&expected, &actual)?;
        }
        Ok(())
    }

    fn current_roots(&self) -> Vec<String> {
        self.graph
            .requested_ports()
            .map(|p| p.origin.clone())
            .collect()
    }

    /// Newly arrived ports must come up in the state their saved files say —
    /// the second-evaluation contract (2b619ab).
    fn check_arrivals(&self, arrived: &[String]) -> Result<(), String> {
        for origin in arrived {
            let Some(spec) = self.world.port(origin) else {
                continue;
            };
            for o in &spec.options {
                let expected = self.expected_from_disk(origin, &o.name, o.default_on);
                let actual = self
                    .graph
                    .real_options()
                    .find(|g| g.port_origin == *origin && g.name == o.name)
                    .map(|g| g.enabled);
                if actual != Some(expected) {
                    return Err(format!(
                        "{origin} arrived with {} = {actual:?}, saved state says {expected}",
                        o.name
                    ));
                }
            }
        }
        Ok(())
    }

    fn resettle_now(&mut self, touched: Vec<String>, action: &str) -> Result<String, String> {
        let outcome = self
            .tree
            .resettle_with(&mut self.graph, &touched, &self.session_opts);
        for (origin, why) in &outcome.failed {
            if !self.model.stale.contains(origin) {
                return Err(format!(
                    "{origin} failed to re-evaluate with the tree intact: {why}"
                ));
            }
        }
        self.check_arrivals(&outcome.arrived)?;
        let pairs = self.read_new_log_pairs();
        let cap = touched.len() + 2 * outcome.arrived.len() + outcome.failed.len() + 2;
        check_evals(&pairs, &self.model.visited, &self.model.stale, cap, action)?;
        self.note_visited(pairs);
        self.model.pending_touched.clear();
        Ok(format!(
            "arrived {:?}, failed {}",
            outcome.arrived,
            outcome.failed.len()
        ))
    }

    // ---------------------------------------------------------- the run

    fn run(mut self, tape: &[Slot]) -> Result<Report, Box<Failure>> {
        for (ordinal, slot) in tape.iter().enumerate() {
            let result = self.execute(slot);
            match result {
                Ok(Some(outcome)) => {
                    self.histogram.get_mut(kind_name(slot.kind)).unwrap().0 += 1;
                    self.trace.push(Step {
                        ordinal,
                        action: kind_name(slot.kind).to_string(),
                        outcome,
                    });
                }
                Ok(None) => {
                    self.histogram.get_mut(kind_name(slot.kind)).unwrap().1 += 1;
                    self.trace.push(Step {
                        ordinal,
                        action: kind_name(slot.kind).to_string(),
                        outcome: "skipped (inapplicable)".into(),
                    });
                    continue;
                }
                Err(message) => {
                    return Err(Box::new(Failure {
                        seed: self.seed,
                        at: ordinal,
                        message,
                        trace: self.trace,
                    }));
                }
            }

            self.absorb();
            if let Err(message) = self.check_invariants(true) {
                return Err(Box::new(Failure {
                    seed: self.seed,
                    at: ordinal,
                    message,
                    trace: self.trace,
                }));
            }
        }

        let executed = self.histogram.values().map(|v| v.0).sum();
        let skipped = self.histogram.values().map(|v| v.1).sum();
        Ok(Report {
            seed: self.seed,
            executed,
            skipped,
            histogram: self.histogram,
            trace: self
                .trace
                .iter()
                .map(|s| format!("#{:04} {} -> {}", s.ordinal, s.action, s.outcome))
                .collect(),
        })
    }

    /// One step. Ok(None) = inapplicable, skipped deterministically.
    fn execute(&mut self, slot: &Slot) -> Result<Option<String>, String> {
        let [a, b, c, _d] = slot.args;
        match slot.kind {
            Kind::Toggle | Kind::DoubleToggle => {
                let toggleable: Vec<usize> = self
                    .graph
                    .visible_rows
                    .iter()
                    .enumerate()
                    .filter_map(|(i, r)| match r.node_id {
                        NodeId::Option(_) => Some(i),
                        _ => None,
                    })
                    .collect();
                if toggleable.is_empty() {
                    return Ok(None);
                }
                let row = toggleable[a as usize % toggleable.len()];
                let NodeId::Option(opt_id) = self.graph.visible_rows[row].node_id else {
                    return Ok(None);
                };
                let (origin, name, group_type, was_on) = {
                    let o = &self.graph.option_nodes[opt_id];
                    (
                        o.port_origin.clone(),
                        o.name.clone(),
                        o.group_type.clone(),
                        o.enabled,
                    )
                };
                let plain = self.world.port(&origin).is_some_and(|p| {
                    p.options.iter().any(|o| {
                        o.name == name
                            && o.group_type == "DEFINE"
                            && o.implies.is_empty()
                            && o.prevents.is_empty()
                            && !p.options.iter().any(|other| {
                                other.implies.contains(&name) || other.prevents.contains(&name)
                            })
                    })
                });
                let before = self.model.states.clone();

                let touched = self.graph.toggle_option(row);
                self.model.pending_touched.extend(touched.iter().cloned());
                self.absorb();

                if group_type == "SINGLE" && was_on {
                    // A refused press must change nothing anywhere.
                    if self.model.states != before {
                        return Err(format!(
                            "refused SINGLE press on {origin}/{name} changed state"
                        ));
                    }
                } else if plain {
                    let now = self.model.states[&origin][&name];
                    if now == was_on {
                        return Err(format!("plain toggle of {origin}/{name} did not flip it"));
                    }
                }

                let mut outcome = format!("{origin}/{name}");
                if slot.kind == Kind::DoubleToggle {
                    let touched = self.graph.toggle_option(row);
                    self.model.pending_touched.extend(touched);
                    outcome.push_str(" twice");
                }
                Ok(Some(outcome))
            }

            Kind::Resettle => {
                let mut touched: Vec<String> = self.model.pending_touched.iter().cloned().collect();
                for origin in &self.model.stale {
                    if self.graph.port_index(origin).is_some() && !touched.contains(origin) {
                        touched.push(origin.clone());
                    }
                }
                if touched.is_empty() {
                    return Ok(None);
                }
                self.resettle_now(touched, "resettle").map(Some)
            }

            Kind::View => {
                let outcome = match a % 5 {
                    0 => {
                        self.graph.hide_optionless = !self.graph.hide_optionless;
                        self.graph.rebuild_visible_rows();
                        format!("hide_optionless={}", self.graph.hide_optionless)
                    }
                    1 => {
                        self.graph.search_query = "p0".into();
                        self.graph.rebuild_visible_rows();
                        "search 'p0'".into()
                    }
                    2 => {
                        self.graph.search_query.clear();
                        self.graph.rebuild_visible_rows();
                        "search cleared".into()
                    }
                    3 => {
                        self.graph.collapse_all();
                        self.graph.expand_all();
                        "collapse+expand all".into()
                    }
                    _ => {
                        self.graph.expand_all();
                        "expand all".into()
                    }
                };
                Ok(Some(outcome))
            }

            Kind::GroupCreate => {
                // Two graph ports sharing an option name, grouped, first
                // member's choices adopted across.
                let mut by_option: HashMap<String, Vec<String>> = HashMap::new();
                for o in self.graph.real_options() {
                    let members = by_option.entry(o.name.clone()).or_default();
                    if !members.contains(&o.port_origin) {
                        members.push(o.port_origin.clone());
                    }
                }
                // Members must share no entangled option: adoption assigns
                // states directly (deliberately, so radio picks survive), so
                // grouping over an implies/prevents endpoint could copy in a
                // combination no toggle could reach.
                let mut shared: Vec<(String, Vec<String>)> = by_option
                    .into_iter()
                    .filter(|(_, m)| m.len() >= 2)
                    .filter(|(_, members)| {
                        members.iter().all(|origin| {
                            let Some(port) = self.world.port(origin) else {
                                return false;
                            };
                            port.options.iter().all(|o| {
                                !self.world.entangled(origin, &o.name)
                                    || members
                                        .iter()
                                        .filter(|m| {
                                            self.world.port(m).is_some_and(|p| {
                                                p.options.iter().any(|po| po.name == o.name)
                                            })
                                        })
                                        .count()
                                        < 2
                            })
                        })
                    })
                    .collect();
                shared.sort();
                if shared.is_empty() {
                    return Ok(None);
                }
                let (opt_name, mut members) = shared.swap_remove(a as usize % shared.len());
                members.truncate(2 + (b as usize % 2));
                let group = format!("g-{opt_name}");
                self.graph.groups.insert(group.clone(), members.clone());
                self.model.groups.insert(group.clone(), members.clone());
                let adopted = self.graph.adopt_group_options(&group, &members[0]);
                self.model.pending_touched.extend(members.iter().cloned());
                Ok(Some(format!("{group} {members:?} adopted {adopted}")))
            }

            Kind::GroupRoundTrip => {
                if self.model.groups.is_empty() {
                    return Ok(None);
                }
                let path = self.io.join("bgone.toml");
                save_groups(&path, &self.model.groups).map_err(|e| e.to_string())?;
                let loaded = Config::load(&path).map_err(|e| e.to_string())?;
                if loaded.groups != self.model.groups {
                    return Err(format!(
                        "groups did not round-trip: saved {:?}, loaded {:?}",
                        self.model.groups, loaded.groups
                    ));
                }
                Ok(Some(format!("{} group(s)", self.model.groups.len())))
            }

            Kind::Export | Kind::ExportDryRun => {
                let dry = slot.kind == Kind::ExportDryRun;
                // Settled first, exactly as Ctrl+S and the exit path do.
                let changed = self.graph.changed_ports();
                let mut note = String::new();
                if !changed.is_empty() {
                    note = self.resettle_now(changed, "export-settle")?;
                }
                let make_conf = self.make_conf();
                let stats = exporter::export_options(
                    &self.graph,
                    &self.options_dir(),
                    dry,
                    Some(&make_conf),
                )
                .map_err(|e| e.to_string())?;
                if dry {
                    return Ok(Some(format!("dry run, {} files", stats.files_written)));
                }

                // The model's view of the disk: live ports now carry their
                // absorbed states; external edits to them are superseded.
                self.absorb();
                let live: Vec<String> = self
                    .model
                    .states
                    .keys()
                    .filter(|o| self.graph.is_live(o))
                    .cloned()
                    .collect();
                let mut superseded = 0;
                for origin in &live {
                    self.model
                        .exported
                        .insert(origin.clone(), self.model.states[origin].clone());
                    let before = self.model.external_edits.len();
                    self.model.external_edits.retain(|(o, _)| o != origin);
                    superseded += before - self.model.external_edits.len();
                }
                self.model.block_globals = unanimity(&self.model.states, &live);

                self.check_disk()?;
                let mut outcome = format!("{} files; {note}", stats.files_written);
                if superseded > 0 {
                    let _ = write!(outcome, "; superseded {superseded} external edit(s)");
                }
                Ok(Some(outcome))
            }

            Kind::Reload => {
                let sys = SystemOptions::load(&self.options_dir(), Some(&self.make_conf()))
                    .map_err(|e| e.to_string())?;
                for (origin, states) in &self.model.exported {
                    for (name, &expected) in states {
                        let got = sys.get_state(origin, name, !expected);
                        if got != expected {
                            return Err(format!(
                                "reload lost {origin}/{name}: expected {expected}, got {got}"
                            ));
                        }
                    }
                }
                Ok(Some(format!("{} port files", self.model.exported.len())))
            }

            Kind::NewSession => {
                let patterns: Vec<String> = if a % 3 == 0 {
                    // A glob over a root's category; the engine expands it
                    // against its own spec, never the resolver's answer.
                    let cat = self.world.roots[0].split('/').next().unwrap().to_string();
                    vec![format!("{cat}/*")]
                } else if a % 3 == 1 {
                    // Duplicate targets must collapse to one entry each.
                    let mut v = self.world.roots.clone();
                    v.extend(self.world.roots.clone());
                    v
                } else {
                    self.world.roots.clone()
                };
                let expected_roots: Vec<String> = if a % 3 == 0 {
                    let prefix = format!("{}/", self.world.roots[0].split('/').next().unwrap());
                    self.world
                        .ports
                        .iter()
                        .map(|p| p.origin.clone())
                        .filter(|o| o.starts_with(&prefix))
                        .collect()
                } else {
                    self.world.roots.clone()
                };

                self.session_opts =
                    SystemOptions::load(&self.options_dir(), Some(&self.make_conf()))
                        .map_err(|e| e.to_string())?;
                let graph = self
                    .tree
                    .build(&patterns, &self.session_opts, false)
                    .map_err(|e| format!("session rebuild failed: {e:#}"))?;
                self.graph = graph;
                self.graph.groups = self.model.groups.clone();
                self.graph.expand_all();

                let pairs = self.read_new_log_pairs();
                let cap = 2 * self.world.ports.len() + 4;
                check_evals(
                    &pairs,
                    &self.model.visited,
                    &self.model.stale,
                    cap,
                    "new-session",
                )?;
                self.note_visited(pairs);

                // Persistence: what the new session holds is what the disk
                // says, option by option — saved value, else global, else
                // default. Unsaved toggles from the old session are expected
                // to be gone; that is what saving is for.
                for spec in &self.world.ports {
                    if self.model.stale.contains(&spec.origin) {
                        continue;
                    }
                    for o in &spec.options {
                        let expected = self.expected_from_disk(&spec.origin, &o.name, o.default_on);
                        let actual = self
                            .graph
                            .real_options()
                            .find(|g| g.port_origin == spec.origin && g.name == o.name)
                            .map(|g| g.enabled);
                        if let Some(actual) = actual {
                            if actual != expected {
                                return Err(format!(
                                    "new session loaded {}/{} as {actual}, disk says {expected}",
                                    spec.origin, o.name
                                ));
                            }
                        }
                    }
                }

                self.model.pending_touched.clear();
                self.model.world_stale = false;
                for root in &expected_roots {
                    if self.graph.port_index(root).is_none() {
                        return Err(format!("requested {root} missing from the new session"));
                    }
                }
                Ok(Some(format!("patterns {patterns:?}")))
            }

            Kind::ExternalEditOptions => {
                let mut origins: Vec<String> = self.model.exported.keys().cloned().collect();
                origins.sort();
                if origins.is_empty() {
                    return Ok(None);
                }
                let origin = origins[a as usize % origins.len()].clone();
                let mut names: Vec<String> = self.model.exported[&origin].keys().cloned().collect();
                names.sort();
                // Plain options only: the product loads saved state verbatim,
                // without re-checking group rules or relations, so an external
                // flip of a SINGLE member or an implies endpoint puts the
                // session in a state no toggle could reach. A real edge — a
                // tree update can change group membership after the file was
                // written — recorded as a finding, not fuzzed into every run.
                names.retain(|n| {
                    !self.world.entangled(&origin, n)
                        && self.world.port(&origin).is_some_and(|p| {
                            p.options
                                .iter()
                                .any(|o| o.name == *n && o.group_type == "DEFINE")
                        })
                });
                if names.is_empty() {
                    return Ok(None);
                }
                let name = names[b as usize % names.len()].clone();
                let states = self.model.exported.get_mut(&origin).unwrap();
                let flipped = !states[&name];
                states.insert(name.clone(), flipped);
                let content = options_file_content(&origin, states);
                let dir = self.options_dir().join(origin.replace('/', "_"));
                fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                fs::write(dir.join("options"), content).map_err(|e| e.to_string())?;
                self.model
                    .external_edits
                    .insert((origin.clone(), name.clone()));
                Ok(Some(format!("{origin}/{name} -> {flipped}")))
            }

            Kind::ExternalGlobals => {
                let name = OPTION_POOL[a as usize % OPTION_POOL.len()].to_string();
                let value = b % 2 == 0;
                self.model.external_globals.insert(name.clone(), value);
                self.rewrite_make_conf_user_area()?;
                Ok(Some(format!("{name}={value}")))
            }

            Kind::PreSeed => {
                let absent: Vec<&SpecPort> = self
                    .world
                    .ports
                    .iter()
                    .filter(|p| {
                        self.graph.port_index(&p.origin).is_none()
                            && !p.options.is_empty()
                            && !self.model.exported.contains_key(&p.origin)
                    })
                    .collect();
                if absent.is_empty() {
                    return Ok(None);
                }
                let spec = absent[a as usize % absent.len()];
                let origin = spec.origin.clone();
                let mut states: HashMap<String, bool> = HashMap::new();
                let mut r = b;
                for o in &spec.options {
                    r = r.rotate_left(7).wrapping_add(0x9e37);
                    // Entangled or grouped options keep their defaults: the
                    // product does not re-enforce relations or group rules on
                    // loaded state, so a random saved value there could hold a
                    // combination no session could reach.
                    let value =
                        if o.group_type != "DEFINE" || self.world.entangled(&origin, &o.name) {
                            o.default_on
                        } else {
                            r % 2 == 0
                        };
                    states.insert(o.name.clone(), value);
                }
                let legacy = c % 2 == 0;
                let content = if legacy {
                    legacy_file_content(&states)
                } else {
                    options_file_content(&origin, &states)
                };
                let dir = self.options_dir().join(origin.replace('/', "_"));
                fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                fs::write(dir.join("options"), content).map_err(|e| e.to_string())?;
                self.model.exported.insert(origin.clone(), states);
                Ok(Some(format!(
                    "{origin} ({})",
                    if legacy {
                        "legacy format"
                    } else {
                        "current format"
                    }
                )))
            }

            Kind::TreeMutate => {
                let idx = a as usize % self.world.ports.len();
                let origin = self.world.ports[idx].origin.clone();
                if b % 2 == 0 {
                    let name = format!("MUT{}", self.mutations);
                    self.mutations += 1;
                    self.tree
                        .add_option(&origin, &name, false, "", "DEFINE", "");
                    self.world.port_mut(&origin).options.push(SpecOption {
                        name,
                        default_on: false,
                        group_type: "DEFINE".into(),
                        group_name: String::new(),
                        implies: Vec::new(),
                        prevents: Vec::new(),
                        deps_on: Vec::new(),
                        deps_off: Vec::new(),
                        hidden: Vec::new(),
                    });
                } else {
                    let target = self.world.ports[c as usize % self.world.ports.len()]
                        .origin
                        .clone();
                    if target == origin {
                        return Ok(None);
                    }
                    self.tree.add_port_dep(&origin, &target);
                    self.world.port_mut(&origin).deps.push(target);
                }
                self.tree.write_out();
                self.bump_makefile(&origin);
                self.model.world_stale = true;
                self.model.visited.clear();
                Ok(Some(format!("{origin} changed")))
            }

            Kind::MakeUnreadable => {
                let Some(candidate) = self.world.unreadable_candidate.clone() else {
                    return Ok(None);
                };
                if self.model.stale.contains(&candidate) {
                    return Ok(None);
                }
                self.tree.make_unreadable(&candidate);
                let _ = fs::remove_file(self.tree.root().join(&candidate).join(".port"));
                self.model.stale.insert(candidate.clone());
                Ok(Some(candidate))
            }

            Kind::RestoreUnreadable => {
                if self.model.stale.is_empty() {
                    return Ok(None);
                }
                let stale: Vec<String> = self.model.stale.iter().cloned().collect();
                for origin in &stale {
                    self.tree.make_readable(origin);
                }
                self.tree.write_out();
                let touched: Vec<String> = stale
                    .iter()
                    .filter(|o| self.graph.port_index(o).is_some())
                    .cloned()
                    .collect();
                // Resettled while the ports still count as stale, so their
                // re-asks — legitimate misses, the failures were never
                // cached — stay exempt from the memo check; cleared after.
                let note = if touched.is_empty() {
                    "nothing to re-ask".to_string()
                } else {
                    self.resettle_now(touched, "restore-resettle")?
                };
                self.model.stale.clear();
                Ok(Some(format!("restored {stale:?}; {note}")))
            }

            Kind::DropCache => {
                let conn =
                    rusqlite::Connection::open(self.tree.db_path()).map_err(|e| e.to_string())?;
                conn.execute_batch("DROP TABLE IF EXISTS reply;")
                    .map_err(|e| e.to_string())?;
                self.model.visited.clear();
                Ok(Some("reply table dropped".into()))
            }
        }
    }

    /// Rebuilds make.conf's user area — sentinel plus the external globals —
    /// while carrying the managed block over untouched, the way a user editing
    /// around the block would.
    fn rewrite_make_conf_user_area(&self) -> Result<(), String> {
        let path = self.make_conf();
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let block: String = {
            let mut lines = Vec::new();
            let mut inside = false;
            for line in existing.lines() {
                if line.trim() == "# BEGIN bgone" {
                    inside = true;
                }
                if inside {
                    lines.push(line);
                }
                if line.trim() == "# END bgone" {
                    inside = false;
                }
            }
            lines.join("\n")
        };
        let mut content = format!("{SENTINEL}\n");
        for (name, value) in &self.model.external_globals {
            let key = if *value {
                "OPTIONS_SET"
            } else {
                "OPTIONS_UNSET"
            };
            let _ = writeln!(content, "{key}+={name}");
        }
        if !block.is_empty() {
            let _ = writeln!(content, "\n{block}");
        }
        fs::write(&path, content).map_err(|e| e.to_string())
    }

    /// The disk after an export: exactly the union of everything ever
    /// exported (bgone never deletes a stranded port's saved choices), each
    /// file complete and equal to the model's view, and make.conf holding one
    /// managed block with the user's content intact.
    fn check_disk(&self) -> Result<(), String> {
        let dir = self.options_dir();
        let mut on_disk: BTreeSet<String> = BTreeSet::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if let Some(idx) = name.find('_') {
                        on_disk.insert(format!("{}/{}", &name[..idx], &name[idx + 1..]));
                    }
                }
            }
        }
        let expected: BTreeSet<String> = self.model.exported.keys().cloned().collect();
        check_live_set(&expected, &on_disk).map_err(|e| format!("options dir vs exports: {e}"))?;

        for (origin, states) in &self.model.exported {
            let path = dir.join(origin.replace('/', "_")).join("options");
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("could not read back {origin}: {e}"))?;
            if content.contains("OPTIONS_FILE_SET") {
                let defined: BTreeSet<String> = states.keys().cloned().collect();
                check_options_file(&content, &defined).map_err(|e| format!("{origin}: {e}"))?;
                for (name, &expected) in states {
                    let wrote_set = content.contains(&format!("OPTIONS_FILE_SET+={name}\n"));
                    if wrote_set != expected {
                        return Err(format!(
                            "{origin}/{name}: disk says {wrote_set}, model says {expected}"
                        ));
                    }
                }
            }
        }

        let make_conf = fs::read_to_string(self.make_conf())
            .map_err(|e| format!("could not read make.conf: {e}"))?;
        check_make_conf(&make_conf, SENTINEL)
    }
}

/// The unanimity fold the exporter documents: a name goes global only where
/// every live port that defines it agrees.
fn unanimity(
    states: &HashMap<String, HashMap<String, bool>>,
    live: &[String],
) -> BTreeMap<String, bool> {
    let mut agreed: BTreeMap<String, Option<bool>> = BTreeMap::new();
    for origin in live {
        for (name, &value) in &states[origin] {
            agreed
                .entry(name.clone())
                .and_modify(|a| {
                    if *a != Some(value) {
                        *a = None;
                    }
                })
                .or_insert(Some(value));
        }
    }
    agreed
        .into_iter()
        .filter_map(|(name, v)| v.map(|v| (name, v)))
        .collect()
}

fn options_file_content(origin: &str, states: &HashMap<String, bool>) -> String {
    let name = origin.split('/').nth(1).unwrap_or(origin);
    let mut names: Vec<&String> = states.keys().collect();
    names.sort();
    let mut out = format!(
        "# externally edited\n_OPTIONS_READ={name}-1.0\n_FILE_COMPLETE_OPTIONS_LIST={}\n",
        names
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    for n in names {
        if states[n] {
            let _ = writeln!(out, "OPTIONS_FILE_SET+={n}");
        } else {
            let _ = writeln!(out, "OPTIONS_FILE_UNSET+={n}");
        }
    }
    out
}

fn legacy_file_content(states: &HashMap<String, bool>) -> String {
    let mut names: Vec<&String> = states.keys().collect();
    names.sort();
    let mut out = String::from("# legacy bgone format\n");
    for n in names {
        if states[n] {
            let _ = writeln!(out, "WITH_{n}=true");
        } else {
            let _ = writeln!(out, "WITHOUT_{n}=true");
        }
    }
    out
}
