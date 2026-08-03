use crate::reader::SystemOptions;
use anyhow::{bail, Result};
use rusqlite::{params, Connection};
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

/// One option as read from the cache: id, name, default state, description,
/// group type and group name.
type OptionRow = (i64, String, bool, String, String, String);

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
    pub visible_rows: Vec<VisibleRow>,
    /// Port origin -> `PKGNAME`-ish label ("nginx-1.24.0"), for the header the
    /// ports framework writes into an options file. Only ever informational.
    pub pkg_names: HashMap<String, String>,
    /// Named sets of ports whose option choices are kept in step. Comes from the
    /// config file and is written back to it; members not in the current list
    /// are carried along untouched.
    pub groups: BTreeMap<String, Vec<String>>,

    pub global_mode: Mode,
    pub last_global_seq: u64,
    pub current_seq: u64,
    pub search_query: String,
}

/// Everything read out of the database for one port before entries are built.
struct Collected {
    resolved: bool,
    options: Vec<OptionRow>,
    /// (option name, dependency origin, applies when the option is *off*)
    option_deps: Vec<(String, String, bool)>,
    requires: Vec<String>,
    implies: HashMap<String, Vec<String>>,
    prevents: HashMap<String, Vec<String>>,
}

impl DependencyGraph {
    pub fn load_from_db(
        conn: &Connection,
        patterns: &[String],
        sys_opts: &SystemOptions,
        ignore_missing: bool,
    ) -> Result<Self> {
        let mut resolved_origins = Vec::new();
        let mut seen = HashSet::new();
        let mut unmatched_patterns = Vec::new();

        // 1. Resolve origins/patterns using SQLite GLOB with LIKE fallback
        for pat in patterns {
            let mut matched = false;

            let mut stmt =
                conn.prepare("SELECT origin FROM ports WHERE origin GLOB ?1 ORDER BY origin")?;
            let rows = stmt.query_map(params![pat], |row| row.get::<_, String>(0))?;

            for origin in rows.flatten() {
                matched = true;
                if seen.insert(origin.clone()) {
                    resolved_origins.push(origin);
                }
            }

            if !matched {
                let like_pat = pat.replace('*', "%").replace('?', "_");
                let mut stmt_like =
                    conn.prepare("SELECT origin FROM ports WHERE origin LIKE ?1 ORDER BY origin")?;
                let rows_like =
                    stmt_like.query_map(params![like_pat], |row| row.get::<_, String>(0))?;
                for origin in rows_like.flatten() {
                    matched = true;
                    if seen.insert(origin.clone()) {
                        resolved_origins.push(origin);
                    }
                }
            }

            if !matched {
                unmatched_patterns.push(pat.clone());
            }
        }

        // Always fail if 0 total ports were matched, regardless of ignore_missing
        if resolved_origins.is_empty() {
            bail!(
            "No matching ports found for pattern(s): '{}'. Run 'bgone index' to index your ports tree.",
            patterns.join("', '")
        );
        }

        // Handle unmatched patterns when at least 1 total port resolved
        if !unmatched_patterns.is_empty() {
            if ignore_missing {
                eprintln!(
                    "[!] Warning: No matching ports found for pattern(s): '{}'",
                    unmatched_patterns.join("', '")
                );
            } else {
                bail!(
                "No matching ports found for pattern(s): '{}'. Run 'bgone index' to index your ports tree.",
                unmatched_patterns.join("', '")
            );
            }
        }

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
            visible_rows: Vec::new(),
            pkg_names: HashMap::new(),
            groups: BTreeMap::new(),
            global_mode: Mode::None,
            last_global_seq: 0,
            current_seq: 0,
            search_query: String::new(),
        };

        graph.load_ports(conn, &resolved_origins, sys_opts)?;

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
        conn: &Connection,
        roots: &[String],
        sys_opts: &SystemOptions,
    ) -> Result<()> {
        let requested: HashSet<&str> = roots.iter().map(|s| s.as_str()).collect();

        let mut collected: HashMap<String, Collected> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();

        for origin in roots {
            if seen.insert(origin.clone()) {
                queue.push_back(origin.clone());
            }
        }

        while let Some(origin) = queue.pop_front() {
            let resolved: bool = conn
                .query_row(
                    "SELECT resolved FROM ports WHERE origin = ?1",
                    params![origin],
                    |row| row.get::<_, i32>(0),
                )
                .map(|v| v == 1)
                .unwrap_or(false);

            let mut stmt = conn.prepare(
                "SELECT o.id, o.name, o.default_on, o.description, o.group_type, o.group_name
                 FROM options o JOIN ports p ON p.id = o.port_id
                 WHERE p.origin = ?1 ORDER BY o.name",
            )?;
            let options: Vec<OptionRow> = stmt
                .query_map(params![origin], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i32>(2)? == 1,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();

            // Option-conditional edges, both polarities in one pass. An `_OFF`
            // edge is followed when its option is *unset*, so both have to be
            // walked here even though only one applies at a time — which of
            // them applies is decided later, by `recompute_live_set`.
            let mut option_deps = Vec::new();
            {
                let mut dep_stmt = conn.prepare(
                    "SELECT DISTINCT t.origin, e.polarity FROM dep_edge e
                     JOIN ports t ON t.id = e.to_port_id
                     WHERE e.via_option_id = ?1 ORDER BY t.origin",
                )?;
                for (option_id, opt_name, ..) in &options {
                    let rows: Vec<(String, String)> = dep_stmt
                        .query_map(params![option_id], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })?
                        .filter_map(|r| r.ok())
                        .collect();
                    for (dep, polarity) in rows {
                        if seen.insert(dep.clone()) {
                            queue.push_back(dep.clone());
                        }
                        option_deps.push((opt_name.clone(), dep, polarity == "OFF"));
                    }
                }
            }

            let mut implies: HashMap<String, Vec<String>> = HashMap::new();
            let mut prevents: HashMap<String, Vec<String>> = HashMap::new();
            {
                let mut imp = conn.prepare(
                    "SELECT o.name, i.implies_name FROM option_implies i
                     JOIN options o ON o.id = i.option_id
                     JOIN ports p ON p.id = o.port_id WHERE p.origin = ?1",
                )?;
                for row in imp
                    .query_map(params![origin], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .flatten()
                {
                    implies.entry(row.0).or_default().push(row.1);
                }
                let mut prv = conn.prepare(
                    "SELECT o.name, x.prevents_name FROM option_prevents x
                     JOIN options o ON o.id = x.option_id
                     JOIN ports p ON p.id = o.port_id WHERE p.origin = ?1",
                )?;
                for row in prv
                    .query_map(params![origin], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .flatten()
                {
                    prevents.entry(row.0).or_default().push(row.1);
                }
            }

            let requires: Vec<String> = {
                let mut req_stmt = conn.prepare(
                    "SELECT DISTINCT t.origin FROM dep_edge e
                     JOIN ports f ON f.id = e.from_port_id
                     JOIN ports t ON t.id = e.to_port_id
                     WHERE f.origin = ?1 AND e.via_option_id IS NULL
                     ORDER BY t.origin",
                )?;
                let rows: Vec<String> = req_stmt
                    .query_map(params![origin], |row| row.get::<_, String>(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                rows
            };
            for dep in &requires {
                if seen.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }

            self.record_pkg_name(conn, &origin)?;

            collected.insert(
                origin,
                Collected {
                    resolved,
                    options,
                    option_deps,
                    requires,
                    implies,
                    prevents,
                },
            );
        }

        // Alphabetised, so the list reads as an index rather than a walk order
        let mut origins: Vec<String> = collected.keys().cloned().collect();
        origins.sort();

        for (index, origin) in origins.iter().enumerate() {
            self.by_origin.insert(origin.clone(), index);
        }

        for origin in &origins {
            let entry = collected.remove(origin).expect("origin was just listed");
            let port_id = self.ports.len();

            let mut option_ids = Vec::new();
            for (_, opt_name, default_state, description, group_type, group_name) in entry.options {
                let initial_enabled = sys_opts.get_state(origin, &opt_name, default_state);

                let by_polarity = |off: bool| -> Vec<String> {
                    entry
                        .option_deps
                        .iter()
                        .filter(|(name, _, is_off)| name == &opt_name && *is_off == off)
                        .map(|(_, dep, _)| dep.clone())
                        .collect()
                };
                let dep_origins = by_polarity(false);
                let dep_origins_off = by_polarity(true);

                // Every dependency was queued during the walk, so it is in
                // by_origin, which was fully populated before this loop.
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
                    name: opt_name.clone(),
                    description,
                    enabled: initial_enabled,
                    initial_enabled,
                    group_type,
                    group_name,
                    is_expanded: false,
                    last_single_seq: 0,
                    subtree_seq: 0,
                    subtree_mode: Mode::None,
                    parent_port: port_id,
                    dep_origins,
                    dep_origins_off,
                    dep_idx,
                    dep_idx_off,
                    implies: entry.implies.get(&opt_name).cloned().unwrap_or_default(),
                    prevents: entry.prevents.get(&opt_name).cloned().unwrap_or_default(),
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

        self.build_required_by();
        Ok(())
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
                port.required_by = entries;
            }
        }
    }

    /// Records the package name the options-file header carries.
    ///
    /// `PKGNAME` now comes straight from the cache, where make put it, so there
    /// is nothing left to reconstruct. The old version stitched one together
    /// from PORTNAME and PORTVERSION when the tree had not been read, which was
    /// wrong for 88% of ports — no PORTREVISION, no PORTEPOCH, and no
    /// USES-synthesised prefix like `py312-`.
    fn record_pkg_name(&mut self, conn: &Connection, origin: &str) -> Result<()> {
        if self.pkg_names.contains_key(origin) {
            return Ok(());
        }

        let pkg_name: Option<String> = conn
            .query_row(
                "SELECT pkgname FROM ports WHERE origin = ?1",
                params![origin],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .filter(|n| !n.trim().is_empty());

        if let Some(pkg_name) = pkg_name {
            self.pkg_names.insert(origin.to_string(), pkg_name);
        }
        Ok(())
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

    pub fn toggle_option(&mut self, row_index: usize) {
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

                // Radios only ever turn on; the rest of the group turns off in
                // response. Pressing one that is already on does nothing.
                let is_radio = group_type == "SINGLE" || group_type == "RADIO";
                if !(is_radio && current_enabled) {
                    self.apply_choice(&origin, &name, !current_enabled || is_radio);
                }
            }
        }
        // An option change can add or strand whole subtrees
        self.recompute_live_set();
        self.rebuild_visible_rows();
    }

    /// Records a choice the user made, on the port they made it on and on every
    /// port grouped with it.
    ///
    /// Groups exist because families like `lang/php8*-extensions` are meant to
    /// be configured alike and drift apart when maintained by hand, so a choice
    /// made on one member is a choice made on all of them. A member that has no
    /// option by that name is left alone rather than guessed at.
    pub fn apply_choice(&mut self, origin: &str, name: &str, enabled: bool) {
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

        for target in targets {
            // Members outside the current list keep whatever they had; the group
            // still names them, and they will follow along when next loaded.
            if let Some(port_id) = self.port_index(&target) {
                self.set_choice_on(port_id, name, enabled);
            }
        }
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

        let (group_type, group_name) = {
            let opt = &self.option_nodes[opt_id];
            (opt.group_type.clone(), opt.group_name.clone())
        };

        if group_type == "SINGLE" || group_type == "RADIO" {
            if !enabled {
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

        if enabled {
            self.enforce_implications(port_id, opt_id);
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
                    self.option_nodes[id].enabled = true;
                    if seen.insert(id) {
                        queue.push_back(id);
                    }
                }
            }
            // A prevented option is turned off rather than refused: the press
            // said what the user wants, and this is the half of it that follows.
            for name in prevents {
                if let Some(id) = find(self, &name) {
                    self.option_nodes[id].enabled = false;
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

        for port_id in 0..self.ports.len() {
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
