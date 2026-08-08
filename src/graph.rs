use crate::oracle::{Options, Oracle, Question};
use crate::reader::SystemOptions;
use crate::resolve::{OptionFacts, Polarity, PortFacts};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Row a `Shift + Down` should land on: the next sibling of `from`, or — when
/// `from` is the last child of its parent — the following "uncle", i.e. the next
/// row shallower than `from`. Returns `from` unchanged when neither exists.
///
/// Both cases reduce to "the first following row at equal or shallower depth",
/// because a shallower row always separates two different parents' children.
pub fn next_sibling_index(depths: &[usize], from: usize) -> usize {
    let depth = match depths.get(from) {
        Some(&d) => d,
        None => return from,
    };

    depths
        .iter()
        .enumerate()
        .skip(from + 1)
        .find(|(_, &d)| d <= depth)
        .map(|(i, _)| i)
        .unwrap_or(from)
}

/// Mirror of [`next_sibling_index`] for `Shift + Up`: the previous sibling, or
/// the parent when `from` is the first child.
pub fn prev_sibling_index(depths: &[usize], from: usize) -> usize {
    let depth = match depths.get(from) {
        Some(&d) => d,
        None => return from,
    };

    (0..from)
        .rev()
        .find(|&i| depths[i] <= depth)
        .unwrap_or(from)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Expand,
    Collapse,
    None,
}

/// Why a port is in the list. A port named on the command line stays
/// `Requested` even when something else depends on it as well — asking for a
/// port by name outranks having it dragged in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Requested,
    Dependency,
}

/// The two relationship sections shown beneath a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// Ports pulled in whatever the options say.
    Requires,
    /// Ports that pull this one in, by option or unconditionally.
    RequiredBy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeId {
    Port(usize),
    Option(usize),
    Section {
        port: usize,
        kind: SectionKind,
    },
    /// A jump target. The origin it points at lives in the row's `RowKind`.
    Ref,
    Info,
}

#[derive(Debug, Clone)]
pub enum RowKind {
    Port {
        origin: String,
        provenance: Provenance,
        /// False when `make` could not evaluate this port at index time.
        ///
        /// Such a port has no options through no fault of its own, and without
        /// saying so it is indistinguishable from one that genuinely defines
        /// none — which is the silent gap this whole resolver exists to remove.
        resolved: bool,
    },
    Option {
        name: String,
        description: String,
        enabled: bool,
        group_type: String,
        group_name: String,
    },
    /// A port the option above it pulls in.
    DependsOn {
        origin: String,
        /// False when nothing currently pulls this port in, so it is not in the
        /// list and turning the option on is what would add it.
        active: bool,
    },
    SectionHeader {
        kind: SectionKind,
        count: usize,
    },
    RequiresEntry {
        origin: String,
    },
    RequiredByEntry {
        origin: String,
        /// Set when that port depends on this one only because an option is on.
        via_option: Option<String>,
    },
    Info {
        message: String,
    },
}

impl RowKind {
    /// The port a row points at, for rows that are jump targets.
    pub fn jump_target(&self) -> Option<&str> {
        match self {
            RowKind::DependsOn { origin, .. }
            | RowKind::RequiresEntry { origin }
            | RowKind::RequiredByEntry { origin, .. } => Some(origin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VisibleRow {
    pub depth: usize,
    pub kind: RowKind,
    pub is_expanded: bool,
    pub has_children: bool,
    pub node_id: NodeId,
}

/// A row named in terms that survive the row list being rebuilt.
///
/// Row *numbers* do not survive: expanding or collapsing renumbers everything
/// below the change, so holding the number still slides the highlight onto
/// something unrelated. These identities are indices into `ports` and
/// `option_nodes`, which a rebuild does not touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowAnchor {
    Port(usize),
    Option(usize),
    Section { port: usize, kind: SectionKind },
}

/// One entry in a port's `required by` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredBy {
    pub origin: String,
    pub via_option: Option<String>,
}

/// Expansion state for a relationship section, which has no node of its own.
#[derive(Debug, Clone, Copy, Default)]
pub struct SectionState {
    pub is_expanded: bool,
    pub last_single_seq: u64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct OptionNode {
    pub id: usize,
    pub port_origin: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// State this option had when the list was loaded, used to detect unsaved edits.
    pub initial_enabled: bool,
    pub group_type: String,
    pub group_name: String,
    pub is_expanded: bool,
    pub last_single_seq: u64,
    pub subtree_seq: u64,
    pub subtree_mode: Mode,
    pub parent_port: usize,
    /// Ports this option pulls in when it is *on*, as origins rather than
    /// indices: every port lives once in the flat list and is reached by
    /// jumping, not by nesting.
    pub dep_origins: Vec<String>,
    /// Ports pulled in when the option is *off* — the `FOO_RUN_DEPENDS_OFF`
    /// forms. Rare, and invisible to the parser this replaced, but a port that
    /// substitutes one library for another depending on an option has them.
    pub dep_origins_off: Vec<String>,
    /// `dep_origins` resolved to `ports` indices, so the reachability walk that
    /// runs on every toggle never has to hash a string.
    dep_idx: Vec<usize>,
    /// As `dep_idx`, for the off-polarity edges.
    dep_idx_off: Vec<usize>,
    /// Options this one turns on with it, and ones it cannot coexist with —
    /// `FOO_IMPLIES` and `FOO_PREVENTS`. Enforced on toggle, the way
    /// `bsd.options.mk` does.
    pub implies: Vec<String>,
    pub prevents: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PortEntry {
    pub id: usize,
    pub origin: String,
    pub provenance: Provenance,
    /// Whether `make` could evaluate this port when the cache was built.
    pub resolved: bool,
    pub options: Vec<usize>,
    /// Dependencies pulled in whatever the options say.
    pub requires: Vec<String>,
    /// `requires` resolved to `ports` indices. See [`OptionNode::dep_idx`].
    requires_idx: Vec<usize>,
    /// Inverse of every forward edge, both conditional and unconditional.
    pub required_by: Vec<RequiredBy>,
    pub is_expanded: bool,
    pub last_single_seq: u64,
    pub subtree_seq: u64,
    pub subtree_mode: Mode,
    pub requires_section: SectionState,
    pub required_by_section: SectionState,
}

/// Every distinct port reachable from the requested targets, held once each.
///
/// The list is flat and alphabetised rather than nested. A port depended on by
/// several others used to appear once per edge, which meant either repeating its
/// subtree at every occurrence — enumerating paths rather than edges, which does
/// not scale — or drawing it at one occurrence and not the others, which reads
/// inconsistently. Holding each port once removes the choice: relationships are
/// shown as references that jump to the single entry for that port.
#[derive(Debug)]
pub struct DependencyGraph {
    pub root_origin: String,
    pub ports: Vec<PortEntry>,
    pub option_nodes: Vec<OptionNode>,
    /// Origin -> index into `ports`, for resolving jump targets.
    pub by_origin: HashMap<String, usize>,
    /// Ports something currently pulls in: the requested ports, plus whatever is
    /// reachable from them through *enabled* options and unconditional
    /// dependencies. Turning an option off can drop a port out of this set, and
    /// everything only that port pulled in with it.
    ///
    /// Ports outside it stay in `ports` with their option state intact, so
    /// turning the option back on restores the selections rather than resetting
    /// them. They are neither listed nor written.
    ///
    /// Indexed by position in `ports`; use [`DependencyGraph::is_live`].
    live: Vec<bool>,
    /// Indices into `ports`, alphabetised by origin.
    ///
    /// The list reads as an index, but a port discovered mid-session — because
    /// an option was turned on and make reported something new — has to be
    /// appended, since every existing index would otherwise shift. So order is
    /// kept here rather than in `ports` itself.
    order: Vec<usize>,
    pub visible_rows: Vec<VisibleRow>,
    /// Port origin -> `PKGNAME`-ish label ("nginx-1.24.0"), for the header the
    /// ports framework writes into an options file. Only ever informational.
    pub pkg_names: HashMap<String, String>,
    /// Named sets of ports whose option choices are kept in step. Comes from the
    /// config file and is written back to it; members not in the current list
    /// are carried along untouched.
    pub groups: BTreeMap<String, Vec<String>>,
    /// Leave out ports that define no options at all.
    ///
    /// A build set is mostly leaf libraries with nothing to decide; hiding them
    /// leaves the list showing only what there is a choice about. Ports `make`
    /// could not read are kept either way — they have no options *known*, which
    /// is the one thing that must not be hidden.
    pub hide_optionless: bool,

    pub global_mode: Mode,
    pub last_global_seq: u64,
    pub current_seq: u64,
    pub search_query: String,
}

/// Everything learned about one port before entries are built.
struct Collected {
    resolved: bool,
    /// Option metadata, from the evaluation *as the port ships* — the only one
    /// whose `default_on` means what it says.
    options: Vec<OptionFacts>,
    /// (option name, dependency origin, applies when the option is *off*)
    option_deps: Vec<(String, String, bool)>,
    /// Pulled in whatever the options say, under the set actually in force.
    requires: Vec<String>,
    /// The state each option is in, which decides which `option_deps` apply.
    states: HashMap<String, bool>,
}

/// What a [`DependencyGraph::resettle`] did: which ports arrived, and which
/// could not be re-asked.
#[derive(Debug, Default)]
pub struct ResettleOutcome {
    /// Ports that were not in the build until the resettle ran.
    pub arrived: Vec<String>,
    /// Ports whose re-evaluation failed, with the reason. Their entries are
    /// marked unevaluated rather than left silently holding stale dependencies.
    pub failed: Vec<(String, String)>,
}

impl Collected {
    /// A port `make` could not read. It still exists and is still depended on,
    /// so it is kept and marked rather than dropped — a silently optionless port
    /// is indistinguishable from one that genuinely has none, which is the gap
    /// this whole resolver exists to close.
    fn unevaluated() -> Self {
        Self {
            resolved: false,
            options: Vec::new(),
            option_deps: Vec::new(),
            requires: Vec::new(),
            states: HashMap::new(),
        }
    }

    /// The ports this one pulls in *as currently configured*.
    ///
    /// Only these are walked. A dependency behind an option that is off is not
    /// part of the build, and evaluating it would cost a `make` run for a port
    /// nothing is going to write — turning the option on re-resolves and finds
    /// it then. Its name is still recorded, which is all a "not pulled in" row
    /// needs.
    fn dependencies(&self) -> Vec<String> {
        let mut out = self.requires.clone();
        for (opt, dep, applies_when_off) in &self.option_deps {
            let on = self.states.get(opt).copied().unwrap_or(false);
            if on != *applies_when_off {
                out.push(dep.clone());
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
impl DependencyGraph {
    /// Builds a graph straight from a set of facts, without a ports tree.
    ///
    /// For tests whose subject is the interface rather than resolution: they
    /// need a list with ports and options in it, not a `make` to produce one.
    /// Resolution itself is covered against a real stub tree in
    /// `tests/integration_tests.rs`.
    pub(crate) fn from_facts(
        facts: &[PortFacts],
        requested: &[&str],
        sys_opts: &SystemOptions,
    ) -> Self {
        let mut graph = Self {
            root_origin: requested.join(", "),
            ports: Vec::new(),
            option_nodes: Vec::new(),
            by_origin: HashMap::new(),
            live: Vec::new(),
            order: Vec::new(),
            visible_rows: Vec::new(),
            pkg_names: HashMap::new(),
            groups: BTreeMap::new(),
            hide_optionless: false,
            global_mode: Mode::None,
            last_global_seq: 0,
            current_seq: 0,
            search_query: String::new(),
        };

        for f in facts {
            graph.add_port(f, sys_opts);
        }
        for f in facts {
            graph.apply_resolution(&f.origin, f);
        }
        for origin in requested {
            if let Some(id) = graph.port_index(origin) {
                graph.ports[id].provenance = Provenance::Requested;
                graph.ports[id].is_expanded = true;
            }
        }

        graph.settle_batch();
        graph.rebuild_visible_rows();
        graph
    }
}

/// Whether a pattern is matched against the tree listing rather than taken as
/// a literal origin. One predicate, because the routing decision is made in
/// two places and they drifted once: `[` was routed to the glob path years
/// before the matcher understood it, so any class pattern matched nothing.
fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '['])
}

/// One matchable element of a glob pattern.
enum GlobTok {
    Literal(char),
    /// `?` — any one character.
    Any,
    /// `*` — any run of characters, `/` included.
    Star,
    /// `[...]` — one character from a set, `[!...]`/`[^...]` its complement.
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

enum ClassItem {
    Char(char),
    Range(char, char),
}

impl GlobTok {
    fn matches(&self, c: char) -> bool {
        match self {
            GlobTok::Literal(l) => *l == c,
            GlobTok::Any => true,
            GlobTok::Star => false,
            GlobTok::Class { negated, items } => {
                let hit = items.iter().any(|item| match item {
                    ClassItem::Char(m) => *m == c,
                    ClassItem::Range(a, b) => (*a..=*b).contains(&c),
                });
                hit != *negated
            }
        }
    }
}

/// Parses a `[...]` starting at `open`, returning the token and the index just
/// past the closing `]` — or `None` when the class never closes, in which case
/// the `[` is a literal, as `sh` treats it.
fn parse_class(chars: &[char], open: usize) -> Option<(GlobTok, usize)> {
    let mut i = open + 1;
    let negated = matches!(chars.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        // A `]` first in the class is a member, not the close — `[]a]` is
        // how sh spells "a right bracket or an a".
        if c == ']' && !first {
            return Some((GlobTok::Class { negated, items }, i + 1));
        }
        first = false;
        // `a-z` is a range unless the `-` is last in the class, where it is
        // itself a member.
        if chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_some_and(|&e| e != ']') {
            items.push(ClassItem::Range(c, chars[i + 2]));
            i += 3;
        } else {
            items.push(ClassItem::Char(c));
            i += 1;
        }
    }
    None
}

fn parse_glob(pattern: &str) -> Vec<GlobTok> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                toks.push(GlobTok::Star);
                i += 1;
            }
            '?' => {
                toks.push(GlobTok::Any);
                i += 1;
            }
            '[' => match parse_class(&chars, i) {
                Some((tok, next)) => {
                    toks.push(tok);
                    i = next;
                }
                None => {
                    toks.push(GlobTok::Literal('['));
                    i += 1;
                }
            },
            c => {
                toks.push(GlobTok::Literal(c));
                i += 1;
            }
        }
    }
    toks
}

/// Matches a glob pattern against a port origin.
///
/// `*` spans anything including `/`, so `www/py-*` and `*postgres*` both work;
/// `?` is one character; `[dr]`, `[a-c]` and `[!x]` are sh-style classes.
/// Applied to the tree listing rather than handed to SQLite, now that there is
/// no table of ports to run `GLOB` against.
fn glob_matches(pattern: &str, text: &str) -> bool {
    // Iterative rather than recursive: `star` remembers where the last `*` was,
    // so a mismatch backtracks to just after what that `*` had consumed instead
    // of unwinding a call stack.
    let p = parse_glob(pattern);
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && p[pi].matches(t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && matches!(p[pi], GlobTok::Star) {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }

    p[pi..].iter().all(|tok| matches!(tok, GlobTok::Star))
}

impl DependencyGraph {
    /// Builds the list by asking the tree, starting from whatever `patterns`
    /// name.
    ///
    /// A pattern without a metacharacter is taken at its word rather than
    /// matched against the tree listing, so naming one port does not cost a walk
    /// of 34,954 directories.
    pub fn resolve(
        oracle: &Oracle,
        patterns: &[String],
        sys_opts: &SystemOptions,
        ignore_missing: bool,
    ) -> Result<Self> {
        let mut resolved_origins = Vec::new();
        let mut seen = HashSet::new();
        let mut unmatched_patterns = Vec::new();

        // An unreadable tree is its own error, not "no matching ports": a glob
        // against a listing that could not be read matches nothing for a
        // reason the message has to name.
        let needs_listing = patterns.iter().any(|p| is_glob(p));
        let listing = if needs_listing {
            oracle.enumerate()?
        } else {
            Vec::new()
        };

        for pat in patterns {
            let mut matched = false;

            if is_glob(pat) {
                for origin in listing.iter().filter(|o| glob_matches(pat, o)) {
                    matched = true;
                    if seen.insert(origin.clone()) {
                        resolved_origins.push(origin.clone());
                    }
                }
            } else if oracle.ports_dir().join(pat).join("Makefile").is_file()
                || !oracle.tree_readable()
            {
                // Without a tree there is nothing to check the name against, so
                // it is taken as given and fails later if the memo has never
                // heard of it — with a message that says so.
                matched = true;
                if seen.insert(pat.clone()) {
                    resolved_origins.push(pat.clone());
                }
            }

            if !matched {
                unmatched_patterns.push(pat.clone());
            }
        }

        if resolved_origins.is_empty() {
            bail!(
                "No matching ports found for pattern(s): '{}' under {}",
                patterns.join("', '"),
                oracle.ports_dir().display()
            );
        }

        if !unmatched_patterns.is_empty() {
            if ignore_missing {
                eprintln!(
                    "[!] Warning: No matching ports found for pattern(s): '{}'",
                    unmatched_patterns.join("', '")
                );
            } else {
                bail!(
                    "No matching ports found for pattern(s): '{}' under {}",
                    unmatched_patterns.join("', '"),
                    oracle.ports_dir().display()
                );
            }
        }

        resolved_origins.sort();

        let header_title = if resolved_origins.len() == 1 {
            resolved_origins[0].clone()
        } else {
            format!(
                "{} ports matched ({})",
                resolved_origins.len(),
                patterns.join(", ")
            )
        };

        let mut graph = Self {
            root_origin: header_title,
            ports: Vec::new(),
            option_nodes: Vec::new(),
            by_origin: HashMap::new(),
            live: Vec::new(),
            order: Vec::new(),
            visible_rows: Vec::new(),
            pkg_names: HashMap::new(),
            groups: BTreeMap::new(),
            hide_optionless: false,
            global_mode: Mode::None,
            last_global_seq: 0,
            current_seq: 0,
            search_query: String::new(),
        };

        let failures = graph.load_ports(oracle, &resolved_origins, sys_opts)?;

        // One port among many that make could not read is a real port with
        // unknown options, and the list says so about it. *Every* named port
        // failing is a different thing — the tree is wrong, or unreachable, or
        // the names are — and continuing would show an empty list with no
        // explanation. So the first reason is reported instead of swallowed.
        let all_unread = resolved_origins.iter().all(|o| {
            graph
                .port_index(o)
                .map(|id| !graph.ports[id].resolved)
                .unwrap_or(true)
        });
        if all_unread {
            match failures.first() {
                Some((_, why)) => bail!(
                    "Could not evaluate any of '{}' under {}: {}",
                    patterns.join("', '"),
                    oracle.ports_dir().display(),
                    why
                ),
                None => bail!(
                    "No matching ports found for pattern(s): '{}' under {}",
                    patterns.join("', '"),
                    oracle.ports_dir().display()
                ),
            }
        }

        graph.recompute_live_set();
        graph.rebuild_visible_rows();

        Ok(graph)
    }

    /// Walks the dependency graph breadth-first from `roots` and builds one
    /// entry per distinct port.
    ///
    /// Termination is by reaching a port already collected, which a finite ports
    /// tree guarantees — there is no depth limit. The walk is iterative because
    /// dependency chains are unbounded and a recursive one would put them on the
    /// call stack.
    fn load_ports(
        &mut self,
        oracle: &Oracle,
        roots: &[String],
        sys_opts: &SystemOptions,
    ) -> Result<Vec<(String, String)>> {
        let mut failures: Vec<(String, String)> = Vec::new();
        let requested: HashSet<&str> = roots.iter().map(|s| s.as_str()).collect();

        let mut collected: HashMap<String, Collected> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut level: Vec<String> = Vec::new();

        for origin in roots {
            if seen.insert(origin.clone()) {
                level.push(origin.clone());
            }
        }

        // Level by level rather than one port at a time: a port's dependencies
        // are not known until it has been evaluated, so the walk cannot run
        // ahead of itself — but everything at the same distance from the roots
        // is independent, and evaluating a level takes as long as its slowest
        // port rather than the sum of them.
        while !level.is_empty() {
            let answers = oracle.facts_many(
                &level
                    .iter()
                    .map(|o| (o.clone(), Options::AsShipped))
                    .collect::<Vec<_>>(),
            );

            let mut next: Vec<String> = Vec::new();
            for (origin, answer) in answers {
                let facts = match answer {
                    Ok(facts) => facts,
                    Err(e) => {
                        // A port make could not read still exists and is still
                        // depended on. It is recorded as unevaluated so the list
                        // can say so, rather than quietly showing a port with no
                        // options — but the reason is kept, because "no options"
                        // and "could not ask" are different problems.
                        failures.push((origin.clone(), e.to_string()));
                        collected.insert(origin, Collected::unevaluated());
                        continue;
                    }
                };

                // The second evaluation — under the options in force — can
                // fail on its own, and one port's failure must not abort the
                // walk any more than a first-evaluation failure does. The two
                // paths record the same way.
                let entry = match self.collect_port(oracle, &facts, sys_opts) {
                    Ok(entry) => entry,
                    Err(e) => {
                        failures.push((origin.clone(), e.to_string()));
                        collected.insert(origin, Collected::unevaluated());
                        continue;
                    }
                };
                for dep in entry.dependencies() {
                    if seen.insert(dep.clone()) {
                        next.push(dep);
                    }
                }
                if !facts.pkgname.trim().is_empty() {
                    self.pkg_names.insert(origin.clone(), facts.pkgname.clone());
                }
                collected.insert(origin, entry);
            }
            level = next;
        }

        // Alphabetised, so the list reads as an index rather than a walk order
        let mut origins: Vec<String> = collected.keys().cloned().collect();
        origins.sort();

        for (index, origin) in origins.iter().enumerate() {
            self.by_origin.insert(origin.clone(), index);
        }

        for origin in &origins {
            let mut entry = collected.remove(origin).expect("origin was just listed");
            let port_id = self.ports.len();

            // Options come out of the cache in name order, which scatters a
            // category's members through the list — an OPTIONS_SINGLE whose two
            // choices sit ten rows apart reads as two unrelated switches rather
            // than as a choice between them. Ungrouped options first, then each
            // category whole, so the `<group>` badge marks a run rather than an
            // isolated row.
            entry.options.sort_by(|a, b| {
                let key = |o: &OptionFacts| {
                    (
                        !o.group_name.is_empty(),
                        o.group_name.clone(),
                        o.group_type.clone(),
                        o.name.clone(),
                    )
                };
                key(a).cmp(&key(b))
            });

            let mut option_ids = Vec::new();
            for opt in entry.options {
                let initial_enabled = entry
                    .states
                    .get(&opt.name)
                    .copied()
                    .unwrap_or(opt.default_on);

                let by_polarity = |off: bool| -> Vec<String> {
                    entry
                        .option_deps
                        .iter()
                        .filter(|(name, _, is_off)| name == &opt.name && *is_off == off)
                        .map(|(_, dep, _)| dep.clone())
                        .collect()
                };
                let dep_origins = by_polarity(false);
                let dep_origins_off = by_polarity(true);

                // A dependency behind an option that is off was never walked, so
                // it has no index — which is exactly right: it is not in the
                // build, and turning the option on re-resolves and finds it.
                let resolve_idx = |origins: &[String]| -> Vec<usize> {
                    origins
                        .iter()
                        .filter_map(|d| self.by_origin.get(d).copied())
                        .collect()
                };
                let dep_idx = resolve_idx(&dep_origins);
                let dep_idx_off = resolve_idx(&dep_origins_off);

                let opt_id = self.option_nodes.len();
                self.option_nodes.push(OptionNode {
                    id: opt_id,
                    port_origin: origin.clone(),
                    name: opt.name.clone(),
                    description: opt.description,
                    enabled: initial_enabled,
                    initial_enabled,
                    group_type: opt.group_type,
                    group_name: opt.group_name,
                    is_expanded: false,
                    last_single_seq: 0,
                    subtree_seq: 0,
                    subtree_mode: Mode::None,
                    parent_port: port_id,
                    dep_origins,
                    dep_origins_off,
                    dep_idx,
                    dep_idx_off,
                    implies: opt.implies,
                    prevents: opt.prevents,
                });
                option_ids.push(opt_id);
            }

            let provenance = if requested.contains(origin.as_str()) {
                Provenance::Requested
            } else {
                Provenance::Dependency
            };

            let requires_idx: Vec<usize> = entry
                .requires
                .iter()
                .filter_map(|d| self.by_origin.get(d).copied())
                .collect();

            self.ports.push(PortEntry {
                id: port_id,
                origin: origin.clone(),
                provenance,
                resolved: entry.resolved,
                options: option_ids,
                requires: entry.requires,
                requires_idx,
                required_by: Vec::new(),
                // Ports asked for by name open on arrival; the rest would bury
                // them, so they start closed and are opened or jumped to.
                is_expanded: provenance == Provenance::Requested,
                last_single_seq: 0,
                subtree_seq: 0,
                subtree_mode: Mode::None,
                requires_section: SectionState::default(),
                required_by_section: SectionState::default(),
            });
        }

        self.reindex();
        Ok(failures)
    }

    /// Recomputes everything derived from the set of ports: the alphabetical
    /// order, the origin-to-index resolutions, and the inverted edges.
    ///
    /// Called whenever the shape of the graph changes — which now happens
    /// mid-session, because turning an option on can make make report a
    /// dependency nothing had heard of.
    fn reindex(&mut self) {
        self.order = (0..self.ports.len()).collect();
        self.order
            .sort_by(|&a, &b| self.ports[a].origin.cmp(&self.ports[b].origin));

        let of = |origins: &[String], by: &HashMap<String, usize>| -> Vec<usize> {
            origins.iter().filter_map(|d| by.get(d).copied()).collect()
        };

        for i in 0..self.ports.len() {
            let requires = self.ports[i].requires.clone();
            self.ports[i].requires_idx = of(&requires, &self.by_origin);
        }
        for i in 0..self.option_nodes.len() {
            let on = self.option_nodes[i].dep_origins.clone();
            let off = self.option_nodes[i].dep_origins_off.clone();
            self.option_nodes[i].dep_idx = of(&on, &self.by_origin);
            self.option_nodes[i].dep_idx_off = of(&off, &self.by_origin);
        }

        self.build_required_by();
    }

    /// The options currently set on a port, which is the question to ask make
    /// about it.
    pub fn option_set(&self, port_id: usize) -> Vec<String> {
        self.ports[port_id]
            .options
            .iter()
            .filter(|&&id| self.option_nodes[id].enabled)
            .map(|&id| self.option_nodes[id].name.clone())
            .collect()
    }

    /// Records what make said a port depends on under the options now set, and
    /// reports the origins that answer mentions which the graph has never seen.
    ///
    /// Does *not* reindex or recompute reachability: both are O(the whole
    /// graph), and a settle applies hundreds of these in a row. The caller does
    /// it once per batch instead — [`settle_batch`](Self::settle_batch).
    ///
    /// This is what closes the gap a single evaluation leaves. A dependency
    /// added by `${opt}_USES` or an `.if ${PORT_OPTIONS:MFOO}` block does not
    /// exist until the option is set, so it cannot have been walked in advance —
    /// it arrives here, when the option is turned on and the port is asked
    /// again.
    pub fn apply_resolution(&mut self, origin: &str, facts: &PortFacts) -> Vec<String> {
        let Some(port_id) = self.port_index(origin) else {
            return Vec::new();
        };

        let mut requires: Vec<String> = facts
            .deps
            .iter()
            .filter(|d| d.via_option.is_none())
            .map(|d| d.origin.clone())
            .collect();
        requires.sort();
        requires.dedup();
        self.ports[port_id].requires = requires;
        self.ports[port_id].resolved = true;

        for &opt_id in &self.ports[port_id].options.clone() {
            let name = self.option_nodes[opt_id].name.clone();
            let by_polarity = |off: bool| -> Vec<String> {
                let mut out: Vec<String> = facts
                    .deps
                    .iter()
                    .filter(|d| {
                        d.via_option.as_deref() == Some(name.as_str())
                            && (d.polarity == Polarity::Off) == off
                    })
                    .map(|d| d.origin.clone())
                    .collect();
                out.sort();
                out.dedup();
                out
            };
            self.option_nodes[opt_id].dep_origins = by_polarity(false);
            self.option_nodes[opt_id].dep_origins_off = by_polarity(true);
        }

        // Only what is actually pulled in is worth chasing. A dependency behind
        // an option that is off is not in the build, and asking make about it
        // would cost an evaluation for a port nothing is going to write.
        let mut wanted: Vec<String> = self.ports[port_id].requires.clone();
        for &opt_id in &self.ports[port_id].options {
            let opt = &self.option_nodes[opt_id];
            let deps = if opt.enabled {
                &opt.dep_origins
            } else {
                &opt.dep_origins_off
            };
            wanted.extend(deps.iter().cloned());
        }

        let mut unknown: Vec<String> = wanted
            .into_iter()
            .filter(|o| !self.by_origin.contains_key(o))
            .collect();
        unknown.sort();
        unknown.dedup();
        unknown
    }

    /// Asks the tree about every port in `origins` under the options now set,
    /// folds the answers in, and keeps going until nothing new turns up.
    ///
    /// The interface does the same thing a batch at a time off the event loop,
    /// so a keystroke is never waiting on `make`; this is the blocking form, for
    /// the places that need the graph settled before they can act on it — saving,
    /// above all, since what gets written is exactly what is in the build.
    pub fn resettle(
        &mut self,
        oracle: &Oracle,
        sys_opts: &SystemOptions,
        origins: &[String],
    ) -> ResettleOutcome {
        let mut outcome = ResettleOutcome::default();
        let mut asking: Vec<Question> = origins
            .iter()
            .filter_map(|o| {
                self.port_index(o)
                    .map(|id| (o.clone(), Options::Exactly(self.option_set(id))))
            })
            .collect();
        let mut asked: HashSet<String> = HashSet::new();

        while !asking.is_empty() {
            for (origin, _) in &asking {
                asked.insert(origin.clone());
            }
            let answers = oracle.facts_many(&asking);
            let mut next: Vec<Question> = Vec::new();

            for (origin, answer) in answers {
                let facts = match answer {
                    Ok(facts) => facts,
                    Err(e) => {
                        // Recorded, not swallowed: the caller is about to
                        // write files, and a port whose dependencies could not
                        // be refreshed has to be said out loud. Its entry is
                        // marked unevaluated so the list says so too, rather
                        // than showing dependencies that may have moved on.
                        if let Some(id) = self.port_index(&origin) {
                            self.ports[id].resolved = false;
                        }
                        outcome.failed.push((origin, e.to_string()));
                        continue;
                    }
                };
                if self.port_index(&origin).is_none() {
                    self.add_port(&facts, sys_opts);
                    outcome.arrived.push(origin.clone());

                    // The port arrived evaluated as it ships, but `add_port`
                    // seeded its options from the saved configuration — and a
                    // dependency added by `${opt}_USES` or an `.if` block only
                    // exists under the options actually in force. Where the
                    // two differ, it is re-asked exactly as `collect_port`
                    // does on the first walk; without this, a port arriving
                    // mid-session got the as-shipped dependencies, which is
                    // the very gap this module exists to close.
                    let in_force: Vec<String> = facts
                        .options
                        .iter()
                        .filter(|o| sys_opts.get_state(&facts.origin, &o.name, o.default_on))
                        .map(|o| o.name.clone())
                        .collect();
                    let as_shipped: Vec<String> = facts
                        .options
                        .iter()
                        .filter(|o| o.default_on)
                        .map(|o| o.name.clone())
                        .collect();
                    if in_force != as_shipped {
                        next.push((origin.clone(), Options::Exactly(in_force)));
                    }
                }
                for unknown in self.apply_resolution(&origin, &facts) {
                    if asked.insert(unknown.clone()) {
                        // Asked as the port ships: nothing is known about its
                        // options yet, and what they default to is part of the
                        // answer.
                        next.push((unknown, Options::AsShipped));
                    }
                }
            }
            self.settle_batch();
            asking = next;
        }

        self.rebuild_visible_rows();
        outcome
    }

    /// Adds a port the walk had not reached, with its options set the way the
    /// user's saved configuration says.
    ///
    /// Appended rather than inserted in place: every `requires_idx`, `dep_idx`
    /// and `parent_port` is an index into `ports`, so inserting would silently
    /// renumber all of them. `order` carries the alphabetical reading instead.
    pub fn add_port(&mut self, facts: &PortFacts, sys_opts: &SystemOptions) -> usize {
        if let Some(existing) = self.port_index(&facts.origin) {
            return existing;
        }

        let port_id = self.ports.len();
        let mut options = facts.options.clone();
        options.sort_by(|a, b| {
            let key = |o: &OptionFacts| {
                (
                    !o.group_name.is_empty(),
                    o.group_name.clone(),
                    o.group_type.clone(),
                    o.name.clone(),
                )
            };
            key(a).cmp(&key(b))
        });

        let mut option_ids = Vec::new();
        for opt in options {
            let enabled = sys_opts.get_state(&facts.origin, &opt.name, opt.default_on);
            let opt_id = self.option_nodes.len();
            self.option_nodes.push(OptionNode {
                id: opt_id,
                port_origin: facts.origin.clone(),
                name: opt.name,
                description: opt.description,
                enabled,
                initial_enabled: enabled,
                group_type: opt.group_type,
                group_name: opt.group_name,
                is_expanded: false,
                last_single_seq: 0,
                subtree_seq: 0,
                subtree_mode: Mode::None,
                parent_port: port_id,
                dep_origins: Vec::new(),
                dep_origins_off: Vec::new(),
                dep_idx: Vec::new(),
                dep_idx_off: Vec::new(),
                implies: opt.implies,
                prevents: opt.prevents,
            });
            option_ids.push(opt_id);
        }

        self.ports.push(PortEntry {
            id: port_id,
            origin: facts.origin.clone(),
            provenance: Provenance::Dependency,
            resolved: true,
            options: option_ids,
            requires: Vec::new(),
            requires_idx: Vec::new(),
            required_by: Vec::new(),
            is_expanded: false,
            last_single_seq: 0,
            subtree_seq: 0,
            subtree_mode: Mode::None,
            requires_section: SectionState::default(),
            required_by_section: SectionState::default(),
        });
        self.by_origin.insert(facts.origin.clone(), port_id);
        if !facts.pkgname.trim().is_empty() {
            self.pkg_names
                .insert(facts.origin.clone(), facts.pkgname.clone());
        }
        self.live.push(false);
        port_id
    }

    /// Puts the graph back in order after a run of `add_port`/`apply_resolution`.
    ///
    /// Kept separate from those two because it walks every port and every
    /// option: doing it per answer made settling 940 ports take four seconds of
    /// pure bookkeeping, against a few hundred milliseconds of actual lookups.
    pub fn settle_batch(&mut self) {
        self.reindex();
        self.recompute_live_set();
    }

    /// Turns one port's evaluation into an entry, re-evaluating it under the
    /// options actually in force when those differ from the maintainer's.
    ///
    /// This second evaluation is the whole point of asking the tree at all.
    /// `bsd.options.mk` lets a port express a dependency two ways: declaratively,
    /// as `MPI_LIB_DEPENDS`, which any single evaluation reports; and
    /// procedurally, as `MYSQL_USES=mysql` or an `.if ${PORT_OPTIONS:MFOO}`
    /// block, which produces nothing at all unless the option is set *while make
    /// is reading the Makefile*. `mail/sqlgrey` pulls in a MySQL client only the
    /// second way. No one evaluation can describe a port under every set of
    /// options, so the set in force is asked about directly.
    fn collect_port(
        &self,
        oracle: &Oracle,
        shipped: &PortFacts,
        sys_opts: &SystemOptions,
    ) -> Result<Collected> {
        // What the options are actually set to: the maintainer's defaults, with
        // the user's saved choices laid over them.
        let states: HashMap<String, bool> = shipped
            .options
            .iter()
            .map(|opt| {
                let state = sys_opts.get_state(&shipped.origin, &opt.name, opt.default_on);
                (opt.name.clone(), state)
            })
            .collect();

        let in_force: Vec<String> = shipped
            .options
            .iter()
            .filter(|opt| states.get(&opt.name).copied().unwrap_or(false))
            .map(|opt| opt.name.clone())
            .collect();
        let as_shipped: Vec<String> = shipped
            .options
            .iter()
            .filter(|opt| opt.default_on)
            .map(|opt| opt.name.clone())
            .collect();

        // Nothing has been changed away from the defaults, so the evaluation
        // already in hand is the one that applies.
        let effective = if in_force == as_shipped {
            None
        } else {
            Some(oracle.facts(&shipped.origin, &Options::Exactly(in_force))?)
        };
        let deps = &effective.as_ref().unwrap_or(shipped).deps;

        let mut requires: Vec<String> = deps
            .iter()
            .filter(|d| d.via_option.is_none())
            .map(|d| d.origin.clone())
            .collect();
        requires.sort();
        requires.dedup();

        let mut option_deps: Vec<(String, String, bool)> = deps
            .iter()
            .filter_map(|d| {
                d.via_option
                    .as_ref()
                    .map(|opt| (opt.clone(), d.origin.clone(), d.polarity == Polarity::Off))
            })
            .collect();
        option_deps.sort();
        option_deps.dedup();

        Ok(Collected {
            resolved: true,
            // Metadata from the as-shipped evaluation: under an override,
            // `PORT_OPTIONS` is whatever was forced, so `default_on` would
            // report the choice back to us as though it were the default.
            options: shipped.options.clone(),
            option_deps,
            requires,
            states,
        })
    }

    /// Inverts every forward edge so each port can show what pulls it in.
    fn build_required_by(&mut self) {
        let mut inverse: HashMap<String, Vec<RequiredBy>> = HashMap::new();

        for port in &self.ports {
            for &opt_id in &port.options {
                let opt = &self.option_nodes[opt_id];
                for dep in &opt.dep_origins {
                    inverse.entry(dep.clone()).or_default().push(RequiredBy {
                        origin: port.origin.clone(),
                        via_option: Some(opt.name.clone()),
                    });
                }
            }
            for dep in &port.requires {
                inverse.entry(dep.clone()).or_default().push(RequiredBy {
                    origin: port.origin.clone(),
                    via_option: None,
                });
            }
        }

        for port in &mut self.ports {
            if let Some(mut entries) = inverse.remove(&port.origin) {
                entries.sort_by(|a, b| {
                    a.origin
                        .cmp(&b.origin)
                        .then_with(|| a.via_option.cmp(&b.via_option))
                });
                entries.dedup();

                // A parent that needs this port whatever its options say needs
                // it, full stop; saying so again for each option it also names
                // listed the same parent twice, once plainly and once in the
                // colour that means "only while this option is on" — which is
                // not true of it. `None` sorts before `Some`, so the
                // unconditional entry is the one already in hand.
                let mut unconditional = String::new();
                entries.retain(|e| {
                    if e.via_option.is_none() {
                        unconditional.clear();
                        unconditional.push_str(&e.origin);
                        return true;
                    }
                    e.origin != unconditional
                });

                port.required_by = entries;
            }
        }
    }

    /// Ports named on the command line, in list order. These are the roots of
    /// the dependency walk and are always kept.
    pub fn requested_ports(&self) -> impl Iterator<Item = &PortEntry> {
        self.ports
            .iter()
            .filter(|p| p.provenance == Provenance::Requested)
    }

    /// Ports in the list that `make` could not evaluate when the cache was
    /// built, so nothing is known about their options.
    ///
    /// Only live ones count: a port stranded by an option being off is not part
    /// of the build, so it is not a gap in what gets written.
    pub fn unevaluated_ports(&self) -> Vec<&str> {
        self.ports
            .iter()
            .filter(|p| !p.resolved && self.live[p.id])
            .map(|p| p.origin.as_str())
            .collect()
    }

    /// True when something currently pulls this port in.
    pub fn is_live(&self, origin: &str) -> bool {
        self.port_index(origin)
            .map(|i| self.live[i])
            .unwrap_or(false)
    }

    /// A port with nothing to decide about it: it defines no options, and `make`
    /// read it successfully, so that is the whole truth rather than a gap.
    fn is_optionless(&self, port_id: usize) -> bool {
        let port = &self.ports[port_id];
        port.options.is_empty() && port.resolved
    }

    /// Live ports that [`hide_optionless`](Self::hide_optionless) would leave out.
    pub fn optionless_count(&self) -> usize {
        (0..self.ports.len())
            .filter(|&id| self.live[id] && self.is_optionless(id))
            .count()
    }

    /// How many ports the list is actually showing, which a search narrows.
    pub fn listed_port_count(&self) -> usize {
        self.visible_rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Port { .. }))
            .count()
    }

    /// How many ports are currently pulled in.
    pub fn live_count(&self) -> usize {
        self.live.iter().filter(|l| **l).count()
    }

    /// Recomputes which ports are currently pulled in, following only enabled
    /// options plus unconditional dependencies.
    ///
    /// Done from scratch on every toggle rather than incrementally: turning one
    /// option off can strand a whole subtree, and only a fresh walk from the
    /// requested ports can tell what is still reachable by some other route.
    pub fn recompute_live_set(&mut self) {
        let mut live = vec![false; self.ports.len()];
        let mut queue: VecDeque<usize> = VecDeque::new();

        for port in self.requested_ports() {
            if !live[port.id] {
                live[port.id] = true;
                queue.push_back(port.id);
            }
        }

        while let Some(port_id) = queue.pop_front() {
            let port = &self.ports[port_id];

            for &dep in &port.requires_idx {
                if !live[dep] {
                    live[dep] = true;
                    queue.push_back(dep);
                }
            }
            for &opt_id in &port.options {
                let opt = &self.option_nodes[opt_id];
                // An option pulls in one set of ports when it is on and, rarely,
                // a different set when it is off — a port substituting one
                // backend for another has both. Whichever applies is followed.
                let deps = if opt.enabled {
                    &opt.dep_idx
                } else {
                    &opt.dep_idx_off
                };
                for &dep in deps {
                    if !live[dep] {
                        live[dep] = true;
                        queue.push_back(dep);
                    }
                }
            }
        }

        self.live = live;
    }

    /// Names the row at `index` so it can be found again after a rebuild.
    ///
    /// Relationship entries and the "no options" note anchor to the port whose
    /// block they sit in. They are exactly the rows an expand or collapse is
    /// most likely to hide, and the port is where the reader would want to end
    /// up anyway.
    pub fn anchor_at(&self, index: usize) -> Option<RowAnchor> {
        let row = self.visible_rows.get(index)?;
        Some(match row.node_id {
            NodeId::Port(id) => RowAnchor::Port(id),
            NodeId::Option(id) => RowAnchor::Option(id),
            NodeId::Section { port, kind } => RowAnchor::Section { port, kind },
            NodeId::Ref | NodeId::Info => RowAnchor::Port(self.enclosing_port(index)?),
        })
    }

    /// The port whose block the row at `index` belongs to.
    pub fn enclosing_port(&self, index: usize) -> Option<usize> {
        self.visible_rows
            .get(..=index)?
            .iter()
            .rev()
            .find_map(|row| match row.node_id {
                NodeId::Port(id) => Some(id),
                _ => None,
            })
    }

    /// Where `anchor` is now, falling outwards to its port when the row it named
    /// is no longer shown — which is what collapsing something the cursor was
    /// inside does.
    pub fn row_of_anchor(&self, anchor: &RowAnchor) -> Option<usize> {
        let find = |want: NodeId| self.visible_rows.iter().position(|r| r.node_id == want);
        match *anchor {
            RowAnchor::Port(id) => find(NodeId::Port(id)),
            RowAnchor::Option(id) => find(NodeId::Option(id))
                .or_else(|| find(NodeId::Port(self.option_nodes[id].parent_port))),
            RowAnchor::Section { port, kind } => {
                find(NodeId::Section { port, kind }).or_else(|| find(NodeId::Port(port)))
            }
        }
    }

    /// Index into `ports` for an origin, for resolving a jump.
    pub fn port_index(&self, origin: &str) -> Option<usize> {
        self.by_origin.get(origin).copied()
    }

    /// Opens a port, so a jump lands on something readable rather than a bare
    /// collapsed row.
    pub fn open_port(&mut self, port_id: usize) {
        self.current_seq += 1;
        let seq = self.current_seq;
        if let Some(p) = self.ports.get_mut(port_id) {
            p.last_single_seq = seq;
            p.is_expanded = true;
        }
        self.rebuild_visible_rows();
    }

    /// Every port option. Anything deciding what gets written to an options file
    /// goes through here.
    pub fn real_options(&self) -> impl Iterator<Item = &OptionNode> {
        self.option_nodes.iter()
    }

    /// True when any option differs from the state it was loaded with. Toggling
    /// an option back to where it started clears this again.
    /// Ports whose options have been changed this session.
    ///
    /// These are the ones whose dependencies may no longer be what the graph
    /// says, so they are where a settle has to start.
    pub fn changed_ports(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .real_options()
            .filter(|opt| opt.enabled != opt.initial_enabled)
            .map(|opt| opt.port_origin.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// True when any option on a port that will actually be written differs from
    /// the state it was loaded with. Edits to a port nothing pulls in any more
    /// are not saved, so they must not raise the unsaved-changes prompt either.
    pub fn is_dirty(&self) -> bool {
        self.real_options()
            .filter(|opt| self.is_live(&opt.port_origin))
            .any(|opt| opt.enabled != opt.initial_enabled)
    }

    pub fn expand_subtree(&mut self, row_index: usize) {
        self.apply_subtree_mode(row_index, Mode::Expand);
    }

    pub fn collapse_subtree(&mut self, row_index: usize) {
        self.apply_subtree_mode(row_index, Mode::Collapse);
    }

    fn apply_subtree_mode(&mut self, row_index: usize, mode: Mode) {
        if let Some(row) = self.visible_rows.get(row_index) {
            self.current_seq += 1;
            let seq = self.current_seq;

            match row.node_id.clone() {
                NodeId::Port(id) => self.apply_port_subtree_mode(id, mode, seq),
                NodeId::Option(id) => self.apply_option_subtree_mode(id, mode, seq),
                NodeId::Section { port, kind } => self.set_section(port, kind, mode, seq),
                NodeId::Ref | NodeId::Info => {}
            }
        }
        self.rebuild_visible_rows();
    }

    fn apply_port_subtree_mode(&mut self, port_id: usize, mode: Mode, seq: u64) {
        let options = match self.ports.get_mut(port_id) {
            Some(p) => {
                p.subtree_seq = seq;
                p.subtree_mode = mode;
                p.is_expanded = mode == Mode::Expand;
                p.requires_section.is_expanded = mode == Mode::Expand;
                p.requires_section.last_single_seq = seq;
                p.required_by_section.is_expanded = mode == Mode::Expand;
                p.required_by_section.last_single_seq = seq;
                p.options.clone()
            }
            None => return,
        };
        for opt_id in options {
            self.apply_option_subtree_mode(opt_id, mode, seq);
        }
    }

    fn apply_option_subtree_mode(&mut self, opt_id: usize, mode: Mode, seq: u64) {
        if let Some(o) = self.option_nodes.get_mut(opt_id) {
            o.subtree_seq = seq;
            o.subtree_mode = mode;
            o.is_expanded = mode == Mode::Expand;
        }
    }

    fn set_section(&mut self, port_id: usize, kind: SectionKind, mode: Mode, seq: u64) {
        if let Some(p) = self.ports.get_mut(port_id) {
            let state = match kind {
                SectionKind::Requires => &mut p.requires_section,
                SectionKind::RequiredBy => &mut p.required_by_section,
            };
            state.is_expanded = mode == Mode::Expand;
            state.last_single_seq = seq;
        }
    }

    pub fn expand_all(&mut self) {
        self.set_all(Mode::Expand);
    }

    pub fn collapse_all(&mut self) {
        self.set_all(Mode::Collapse);
    }

    fn set_all(&mut self, mode: Mode) {
        self.current_seq += 1;
        self.last_global_seq = self.current_seq;
        self.global_mode = mode;
        let expanded = mode == Mode::Expand;

        for port in &mut self.ports {
            port.is_expanded = expanded;
            port.requires_section.is_expanded = expanded;
            port.required_by_section.is_expanded = expanded;
        }
        for opt in &mut self.option_nodes {
            opt.is_expanded = expanded;
        }

        self.rebuild_visible_rows();
    }

    /// Expands or collapses a single row, leaving its descendants' own
    /// expansion state untouched.
    pub fn set_node_expanded(&mut self, row_index: usize, expanded: bool) {
        if let Some(row) = self.visible_rows.get(row_index) {
            self.current_seq += 1;
            let seq = self.current_seq;

            match row.node_id.clone() {
                NodeId::Port(id) => {
                    if let Some(p) = self.ports.get_mut(id) {
                        p.last_single_seq = seq;
                        p.is_expanded = expanded;
                    }
                }
                NodeId::Option(id) => {
                    if let Some(o) = self.option_nodes.get_mut(id) {
                        o.last_single_seq = seq;
                        o.is_expanded = expanded;
                    }
                }
                NodeId::Section { port, kind } => {
                    let mode = if expanded {
                        Mode::Expand
                    } else {
                        Mode::Collapse
                    };
                    self.set_section(port, kind, mode, seq);
                }
                NodeId::Ref | NodeId::Info => {}
            }
        }
        self.rebuild_visible_rows();
    }

    pub fn expand_node(&mut self, row_index: usize) {
        self.set_node_expanded(row_index, true);
    }

    pub fn collapse_node(&mut self, row_index: usize) {
        self.set_node_expanded(row_index, false);
    }

    /// Turns the option under the cursor on or off, reporting every port whose
    /// options that changed — the one under the cursor, and any grouped with it.
    ///
    /// The callers need that list because a changed option set is a different
    /// question to ask make: what a port depends on is not derivable from what
    /// is already known about it.
    pub fn toggle_option(&mut self, row_index: usize) -> Vec<String> {
        let mut touched = Vec::new();
        if let Some(row) = self.visible_rows.get(row_index) {
            if let NodeId::Option(id) = row.node_id {
                let (origin, name, group_type, current_enabled) = {
                    let opt = &self.option_nodes[id];
                    (
                        opt.port_origin.clone(),
                        opt.name.clone(),
                        opt.group_type.clone(),
                        opt.enabled,
                    )
                };

                // `OPTIONS_SINGLE` must have exactly one member set, so pressing
                // the one already on does nothing — there is nothing to fall
                // back to. `OPTIONS_RADIO` is the optional form, zero or one,
                // and `bsddialog` lets you clear it; so does this.
                if !(group_type == "SINGLE" && current_enabled) {
                    touched = self.apply_choice(&origin, &name, !current_enabled);
                }
            }
        }
        // An option change can add or strand whole subtrees
        self.recompute_live_set();
        self.rebuild_visible_rows();
        touched
    }

    /// Records a choice the user made, on the port they made it on and on every
    /// port grouped with it.
    ///
    /// Groups exist because families like `lang/php8*-extensions` are meant to
    /// be configured alike and drift apart when maintained by hand, so a choice
    /// made on one member is a choice made on all of them. A member that has no
    /// option by that name is left alone rather than guessed at.
    ///
    /// Returns the ports in the current list whose options this actually
    /// changed, which is what has to be asked about again.
    pub fn apply_choice(&mut self, origin: &str, name: &str, enabled: bool) -> Vec<String> {
        let mut targets = vec![origin.to_string()];
        for members in self.groups.values() {
            if members.iter().any(|m| m == origin) {
                for member in members {
                    if !targets.contains(member) {
                        targets.push(member.clone());
                    }
                }
            }
        }

        let mut touched = Vec::new();
        for target in targets {
            // Members outside the current list keep whatever they had; the group
            // still names them, and they will follow along when next loaded.
            if let Some(port_id) = self.port_index(&target) {
                self.set_choice_on(port_id, name, enabled);
                touched.push(target);
            }
        }
        touched
    }

    /// Brings a port that has just joined `group` into line with the members
    /// already in it, reporting how many options it took on.
    ///
    /// Joining a group is a statement that this port should be configured like
    /// its siblings, so it adopts their choices rather than waiting for the next
    /// toggle to reach it — which would otherwise leave the group claiming to be
    /// in step while its newest member disagreed with all of it.
    ///
    /// The first member of a group has nobody to copy, so this does nothing. A
    /// name the joining port does not define is skipped, and one it defines that
    /// nobody else does is left as it was: a group is a set of ports that
    /// *overlap*, not one that matches.
    pub fn adopt_group_options(&mut self, group: &str, origin: &str) -> usize {
        let Some(target_id) = self.port_index(origin) else {
            return 0;
        };
        // Whichever member is listed first and is actually in front of us. The
        // members are meant to agree, so any of them answers the question.
        let Some(source_id) = self
            .groups
            .get(group)
            .into_iter()
            .flatten()
            .filter(|m| m.as_str() != origin)
            .find_map(|m| self.port_index(m))
        else {
            return 0;
        };

        let source: Vec<(String, bool)> = self.ports[source_id]
            .options
            .iter()
            .map(|&id| {
                let opt = &self.option_nodes[id];
                (opt.name.clone(), opt.enabled)
            })
            .collect();

        // Assigned rather than pushed through `set_choice_on`, because the
        // source port is already internally consistent: replaying its choices
        // one at a time would let each radio pick undo the one before it.
        let before: Vec<(usize, bool)> = self.ports[target_id]
            .options
            .iter()
            .map(|&id| (id, self.option_nodes[id].enabled))
            .collect();
        let mut adopted: usize = 0;
        for &opt_id in &self.ports[target_id].options.clone() {
            let name = self.option_nodes[opt_id].name.clone();
            if let Some((_, enabled)) = source.iter().find(|(n, _)| *n == name) {
                if self.option_nodes[opt_id].enabled != *enabled {
                    adopted += 1;
                }
                self.option_nodes[opt_id].enabled = *enabled;
            }
        }

        // The source's consistency is only the target's where they define the
        // same members. A source whose set SINGLE member the target lacks
        // copies `off` onto every shared sibling and empties the target's
        // group — found by the simulated-user engine on its first run. The
        // previously set member is restored: the target was consistent before
        // the copy, and a SINGLE has to keep one.
        let groups: HashSet<(String, String)> = self.ports[target_id]
            .options
            .iter()
            .map(|&id| &self.option_nodes[id])
            .filter(|o| o.group_type == "SINGLE")
            .map(|o| (o.group_type.clone(), o.group_name.clone()))
            .collect();
        for (group_type, group_name) in groups {
            let members: Vec<usize> = self.ports[target_id]
                .options
                .iter()
                .copied()
                .filter(|&id| {
                    let o = &self.option_nodes[id];
                    o.group_type == group_type && o.group_name == group_name
                })
                .collect();
            if members.iter().all(|&id| !self.option_nodes[id].enabled) {
                if let Some(&(id, _)) = before.iter().find(|(id, was)| *was && members.contains(id))
                {
                    self.option_nodes[id].enabled = true;
                    adopted = adopted.saturating_sub(1);
                }
            }
        }

        self.recompute_live_set();
        self.rebuild_visible_rows();
        adopted
    }

    /// Applies a choice to one port, resolved against *that* port's own option
    /// set — its radio grouping may differ from the port the choice came from.
    fn set_choice_on(&mut self, port_id: usize, name: &str, enabled: bool) {
        let Some(opt_id) = self.ports[port_id]
            .options
            .iter()
            .copied()
            .find(|&id| self.option_nodes[id].name == name)
        else {
            return;
        };

        self.apply_state_respecting_groups(port_id, opt_id, enabled);

        if enabled {
            if self.option_nodes[opt_id].enabled {
                self.retire_preventers(port_id, opt_id);
            }
            self.enforce_implications(port_id, opt_id);
        } else if !self.option_nodes[opt_id].enabled {
            // Only when the disable took: a SINGLE's set member refuses, and
            // a refused press must not take its impliers down with it.
            self.retire_impliers(port_id, opt_id);
        }
    }

    /// Follows `FOO_PREVENTS` in the other direction, for an option just
    /// turned on: an enabled option that declares it prevented goes off — the
    /// declaration is symmetric ("cannot coexist"), even though only one side
    /// carries it. Enabling the prevented side used to leave both on, a
    /// combination the framework rejects at build time; the philosophy is the
    /// same as everywhere else in this family — the press said what the user
    /// wants, and this is the half of it that follows.
    ///
    /// A retired preventer takes its own enabled impliers with it, exactly as
    /// a direct press on it would. One that cannot lawfully turn off — a
    /// SINGLE's one set member — keeps its declaration in force, so the
    /// pressed option goes back off and the press is refused.
    fn retire_preventers(&mut self, port_id: usize, opt_id: usize) {
        let name = self.option_nodes[opt_id].name.clone();
        let preventers: Vec<usize> = self.ports[port_id]
            .options
            .iter()
            .copied()
            .filter(|&id| {
                let o = &self.option_nodes[id];
                id != opt_id && o.enabled && o.prevents.contains(&name)
            })
            .collect();

        for id in preventers {
            self.apply_state_respecting_groups(port_id, id, false);
            if !self.option_nodes[id].enabled {
                self.retire_impliers(port_id, id);
            }
        }

        let still_prevented = self.ports[port_id].options.iter().copied().any(|id| {
            let o = &self.option_nodes[id];
            id != opt_id && o.enabled && o.prevents.contains(&name)
        });
        if still_prevented {
            self.apply_state_respecting_groups(port_id, opt_id, false);
        }
    }

    /// Follows `FOO_IMPLIES` in the other direction, for an option just turned
    /// off: every enabled option that implies it goes off too, transitively.
    ///
    /// Without this, enabling NJS (which implies STREAM) and then turning
    /// STREAM off directly left NJS-on/STREAM-off in the session — a
    /// combination `bsd.options.mk` silently overrides at build time, which is
    /// exactly what this program promises cannot be saved. The philosophy is
    /// the PREVENTS one: the press said what the user wants, and this is the
    /// half of it that follows.
    ///
    /// An implier that cannot lawfully turn off — a SINGLE's one set member —
    /// still implies its option, so the pressed option is put back on and the
    /// press is refused, the same way pressing that SINGLE member itself would
    /// have been.
    fn retire_impliers(&mut self, port_id: usize, opt_id: usize) {
        let mut seen: HashSet<usize> = HashSet::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(opt_id);
        seen.insert(opt_id);

        while let Some(current) = queue.pop_front() {
            let current_name = self.option_nodes[current].name.clone();
            let impliers: Vec<usize> = self.ports[port_id]
                .options
                .iter()
                .copied()
                .filter(|&id| {
                    let o = &self.option_nodes[id];
                    o.enabled && o.implies.contains(&current_name)
                })
                .collect();
            for id in impliers {
                self.apply_state_respecting_groups(port_id, id, false);
                if seen.insert(id) {
                    queue.push_back(id);
                }
            }
        }

        // A refusal upstream leaves an enabled implier standing; honouring the
        // press anyway would recreate the very combination this exists to
        // prevent, so the press is undone instead.
        let name = self.option_nodes[opt_id].name.clone();
        let still_implied = self.ports[port_id].options.iter().copied().any(|id| {
            let o = &self.option_nodes[id];
            o.enabled && o.implies.contains(&name)
        });
        if still_implied {
            self.apply_state_respecting_groups(port_id, opt_id, true);
        }
    }

    /// Applies one state to one option under its group's rules: turning a
    /// SINGLE or RADIO member on clears the siblings it displaces, turning a
    /// SINGLE's set member off is refused (the group has to keep one), and a
    /// RADIO clears. Nothing more — implications are the caller's to follow,
    /// which is what keeps a chain of them from recursing through here.
    fn apply_state_respecting_groups(&mut self, port_id: usize, opt_id: usize, enabled: bool) {
        let (group_type, group_name) = {
            let opt = &self.option_nodes[opt_id];
            (opt.group_type.clone(), opt.group_name.clone())
        };

        if group_type == "SINGLE" || group_type == "RADIO" {
            if !enabled {
                // A SINGLE has to keep one member set, so there is nothing to
                // do; a RADIO may have none, and clears.
                if group_type == "RADIO" {
                    self.option_nodes[opt_id].enabled = false;
                }
                return;
            }
            let siblings: Vec<usize> = self.ports[port_id]
                .options
                .iter()
                .copied()
                .filter(|&id| {
                    let s = &self.option_nodes[id];
                    s.group_type == group_type && s.group_name == group_name
                })
                .collect();
            for id in siblings {
                self.option_nodes[id].enabled = id == opt_id;
            }
        } else {
            self.option_nodes[opt_id].enabled = enabled;
        }
    }

    /// Applies `FOO_IMPLIES` and `FOO_PREVENTS` for an option just turned on.
    ///
    /// `bsd.options.mk` enforces both, and so does `bsddialog` — a port that
    /// declares `NJS_IMPLIES=STREAM` cannot be built with NJS on and STREAM off,
    /// so offering that combination would produce an options file the framework
    /// then quietly overrides. Neither relation was modelled before this;
    /// 345 ports declare an implication and 46 a conflict.
    ///
    /// Implications are followed transitively, since an implied option may imply
    /// others; `seen` is what stops a pair of options implying each other from
    /// looping. Only options the same port defines are touched — a port may name
    /// one it does not define, and there is nothing to set in that case.
    ///
    /// Every write goes through the group rules: an implication into a SINGLE
    /// or RADIO displaces the sibling it outranks rather than leaving two set,
    /// and a PREVENTS aimed at a SINGLE's one set member is refused rather than
    /// emptying the group — both combinations the framework will not accept.
    fn enforce_implications(&mut self, port_id: usize, opt_id: usize) {
        let mut seen: HashSet<usize> = HashSet::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(opt_id);
        seen.insert(opt_id);

        while let Some(current) = queue.pop_front() {
            let (implies, prevents) = {
                let opt = &self.option_nodes[current];
                (opt.implies.clone(), opt.prevents.clone())
            };

            let find = |graph: &Self, name: &str| -> Option<usize> {
                graph.ports[port_id]
                    .options
                    .iter()
                    .copied()
                    .find(|&id| graph.option_nodes[id].name == name)
            };

            for name in implies {
                if let Some(id) = find(self, &name) {
                    self.apply_state_respecting_groups(port_id, id, true);
                    if seen.insert(id) {
                        queue.push_back(id);
                    }
                }
            }
            // A prevented option is turned off rather than refused: the press
            // said what the user wants, and this is the half of it that follows.
            // Its own enabled impliers go with it, exactly as a direct press
            // on it would take them.
            for name in prevents {
                if let Some(id) = find(self, &name) {
                    let was_enabled = self.option_nodes[id].enabled;
                    self.apply_state_respecting_groups(port_id, id, false);
                    if was_enabled && !self.option_nodes[id].enabled {
                        self.retire_impliers(port_id, id);
                    }
                }
            }
        }
    }

    /// Sets one option directly, without radio handling and without reaching
    /// any group. [`apply_choice`](Self::apply_choice) is what a user action
    /// goes through; this is the primitive underneath it.
    pub fn set_option_state(&mut self, port_origin: &str, option_name: &str, enabled: bool) {
        for opt in &mut self.option_nodes {
            if opt.port_origin == port_origin && opt.name == option_name {
                opt.enabled = enabled;
            }
        }
    }

    /// Option nodes matching a `(port origin, option name)` pair. Each port is
    /// held once, so this yields at most one.
    #[allow(dead_code)]
    pub fn option_instances(&self, port_origin: &str, option_name: &str) -> Vec<usize> {
        self.option_nodes
            .iter()
            .filter(|o| o.port_origin == port_origin && o.name == option_name)
            .map(|o| o.id)
            .collect()
    }

    pub fn rebuild_visible_rows(&mut self) {
        // Built into a local vector so `flatten_port` can borrow `self`
        // immutably; otherwise every port would have to be cloned to satisfy the
        // borrow checker, which dominates the cost on a large list.
        let mut rows = Vec::with_capacity(self.visible_rows.len());
        let query = self.search_query.to_lowercase();

        for &port_id in &self.order {
            // Searching narrows which *ports* are listed, on their origin alone,
            // and a port that matches is shown whole.
            //
            // Filtering row by row instead severed options from the port they
            // belong to — an option matching on its description survived while
            // its port header did not, leaving orphans under whatever port
            // happened to be above — and left section headers standing with
            // their entries gone.
            if !query.is_empty() && !self.ports[port_id].origin.to_lowercase().contains(&query) {
                continue;
            }
            if self.hide_optionless && self.is_optionless(port_id) {
                continue;
            }
            self.flatten_port(port_id, &mut rows);
        }

        self.visible_rows = rows;
    }

    fn flatten_port(&self, port_id: usize, rows: &mut Vec<VisibleRow>) {
        let port = &self.ports[port_id];
        if !self.live[port_id] {
            return;
        }

        // A parent that is not itself pulled in explains nothing about why this
        // port is here, so it is left out of `required by`.
        let required_by: Vec<&RequiredBy> = port
            .required_by
            .iter()
            .filter(|r| self.is_live(&r.origin))
            .collect();

        let has_children =
            !port.options.is_empty() || !port.requires.is_empty() || !required_by.is_empty();

        rows.push(VisibleRow {
            depth: 0,
            kind: RowKind::Port {
                origin: port.origin.clone(),
                provenance: port.provenance,
                resolved: port.resolved,
            },
            is_expanded: port.is_expanded,
            has_children,
            node_id: NodeId::Port(port_id),
        });

        if !port.is_expanded {
            return;
        }

        if !has_children {
            // Two different silences, and telling them apart is the point: a
            // port that defines nothing is finished, a port make could not read
            // is missing whatever it would have defined.
            let message = if port.resolved {
                "(No options defined for this port)".to_string()
            } else {
                "(make could not evaluate this port - options unknown; re-run 'bgone index')"
                    .to_string()
            };
            rows.push(VisibleRow {
                depth: 1,
                kind: RowKind::Info { message },
                is_expanded: false,
                has_children: false,
                node_id: NodeId::Info,
            });
            return;
        }

        for &opt_id in &port.options {
            let opt = &self.option_nodes[opt_id];
            rows.push(VisibleRow {
                depth: 1,
                kind: RowKind::Option {
                    name: opt.name.clone(),
                    description: opt.description.clone(),
                    enabled: opt.enabled,
                    group_type: opt.group_type.clone(),
                    group_name: opt.group_name.clone(),
                },
                is_expanded: opt.is_expanded,
                has_children: !opt.dep_origins.is_empty(),
                node_id: NodeId::Option(opt_id),
            });

            if opt.is_expanded {
                for dep in &opt.dep_origins {
                    let active = self.is_live(dep);
                    rows.push(VisibleRow {
                        depth: 2,
                        kind: RowKind::DependsOn {
                            origin: dep.clone(),
                            active,
                        },
                        is_expanded: false,
                        has_children: false,
                        node_id: NodeId::Ref,
                    });
                }
            }
        }

        if !port.requires.is_empty() {
            rows.push(VisibleRow {
                depth: 1,
                kind: RowKind::SectionHeader {
                    kind: SectionKind::Requires,
                    count: port.requires.len(),
                },
                is_expanded: port.requires_section.is_expanded,
                has_children: true,
                node_id: NodeId::Section {
                    port: port_id,
                    kind: SectionKind::Requires,
                },
            });

            if port.requires_section.is_expanded {
                for dep in &port.requires {
                    rows.push(VisibleRow {
                        depth: 2,
                        kind: RowKind::RequiresEntry {
                            origin: dep.clone(),
                        },
                        is_expanded: false,
                        has_children: false,
                        node_id: NodeId::Ref,
                    });
                }
            }
        }

        if !required_by.is_empty() {
            rows.push(VisibleRow {
                depth: 1,
                kind: RowKind::SectionHeader {
                    kind: SectionKind::RequiredBy,
                    count: required_by.len(),
                },
                is_expanded: port.required_by_section.is_expanded,
                has_children: true,
                node_id: NodeId::Section {
                    port: port_id,
                    kind: SectionKind::RequiredBy,
                },
            });

            if port.required_by_section.is_expanded {
                for entry in &required_by {
                    rows.push(VisibleRow {
                        depth: 2,
                        kind: RowKind::RequiredByEntry {
                            origin: entry.origin.clone(),
                            via_option: entry.via_option.clone(),
                        },
                        is_expanded: false,
                        has_children: false,
                        node_id: NodeId::Ref,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::glob_matches;

    /// The two metacharacters that have always worked.
    #[test]
    fn stars_and_question_marks_match_as_before() {
        assert!(glob_matches("www/py-*", "www/py-django"));
        assert!(glob_matches("*postgres*", "databases/postgresql16-server"));
        assert!(glob_matches("www/py-?ango", "www/py-dango"));
        assert!(!glob_matches("www/py-?", "www/py-django"));
    }

    /// `[` was routed to the glob path but matched literally, so a class
    /// pattern could never match anything.
    #[test]
    fn character_classes_match_sets_ranges_and_complements() {
        assert!(glob_matches("www/py-[dr]*", "www/py-django"));
        assert!(glob_matches("www/py-[dr]*", "www/py-requests"));
        assert!(!glob_matches("www/py-[dr]*", "www/py-abc"));

        assert!(glob_matches("www/py-[a-c]*", "www/py-abc"));
        assert!(!glob_matches("www/py-[a-c]*", "www/py-django"));

        assert!(glob_matches("www/py-[!dr]*", "www/py-abc"));
        assert!(!glob_matches("www/py-[!dr]*", "www/py-django"));
        assert!(glob_matches("www/py-[^dr]*", "www/py-abc"));
    }

    /// The sh corner cases: a `]` first in a class is a member, a trailing `-`
    /// is a member, and an unclosed `[` is a literal bracket.
    #[test]
    fn class_corner_cases_follow_sh() {
        assert!(glob_matches("a[]x]b", "a]b"));
        assert!(glob_matches("a[]x]b", "axb"));
        assert!(!glob_matches("a[]x]b", "ayb"));

        assert!(glob_matches("a[x-]b", "a-b"));
        assert!(glob_matches("a[x-]b", "axb"));

        assert!(glob_matches("a[b", "a[b"));
        assert!(!glob_matches("a[b", "acb"));
    }
}
