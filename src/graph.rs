use crate::describe::{load_cached, PortDetails};
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

/// One row of the `options` table: name, default state, description, group type
/// and group name.
type OptionRow = (String, bool, String, String, String);

/// Options `bsd.options.mk` turns on by default whenever a port defines them,
/// over and above whatever `OPTIONS_DEFAULT` lists.
const ALWAYS_DEFAULT_ON: [&str; 4] = ["DOCS", "NLS", "EXAMPLES", "IPV6"];

/// Reconciles the option list the regex sweep found with the one the ports tree
/// reports.
///
/// `describe-json` is authoritative for *which* options exist and *which* are on
/// by default, and is the only source that sees options inherited through
/// `MASTERDIR` or injected by `Mk/Uses/*.mk`. It carries no descriptions and no
/// `SINGLE`/`RADIO`/`MULTI` grouping, so those keep coming from the Makefile
/// parse, matched by name. An option only the tree knows about shows up with no
/// description and no group.
fn merge_described_options(indexed: Vec<OptionRow>, details: &PortDetails) -> Vec<OptionRow> {
    if details.complete_options_list.is_empty() {
        return indexed;
    }

    let mut merged: Vec<OptionRow> = details
        .complete_options_list
        .iter()
        .map(|name| {
            let default_on = details.options_default.iter().any(|d| d == name)
                || ALWAYS_DEFAULT_ON.contains(&name.as_str());

            match indexed.iter().find(|(n, ..)| n == name) {
                Some((_, _, description, group_type, group_name)) => (
                    name.clone(),
                    default_on,
                    description.clone(),
                    group_type.clone(),
                    group_name.clone(),
                ),
                None => (
                    name.clone(),
                    default_on,
                    String::new(),
                    "DEFINE".to_string(),
                    String::new(),
                ),
            }
        })
        .collect();

    merged.sort_by(|a, b| a.0.cmp(&b.0));
    merged
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
    /// Ports this option pulls in, as origins rather than indices: every port
    /// lives once in the flat list and is reached by jumping, not by nesting.
    pub dep_origins: Vec<String>,
    /// `dep_origins` resolved to `ports` indices, so the reachability walk that
    /// runs on every toggle never has to hash a string.
    dep_idx: Vec<usize>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PortEntry {
    pub id: usize,
    pub origin: String,
    pub provenance: Provenance,
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
    options: Vec<OptionRow>,
    /// (option name, dependency origin)
    option_deps: Vec<(String, String)>,
    requires: Vec<String>,
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

            for r in rows {
                if let Ok(origin) = r {
                    matched = true;
                    if seen.insert(origin.clone()) {
                        resolved_origins.push(origin);
                    }
                }
            }

            if !matched {
                let like_pat = pat.replace('*', "%").replace('?', "_");
                let mut stmt_like =
                    conn.prepare("SELECT origin FROM ports WHERE origin LIKE ?1 ORDER BY origin")?;
                let rows_like =
                    stmt_like.query_map(params![like_pat], |row| row.get::<_, String>(0))?;
                for r in rows_like {
                    if let Ok(origin) = r {
                        matched = true;
                        if seen.insert(origin.clone()) {
                            resolved_origins.push(origin);
                        }
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
            let mut stmt = conn.prepare(
                "SELECT option_name, default_state, description, group_type, group_name
                 FROM options WHERE port_origin = ?1 ORDER BY option_name",
            )?;
            let indexed: Vec<OptionRow> = stmt
                .query_map(params![origin], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i32>(1)? == 1,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();

            let details = load_cached(conn, &origin);
            let options = match &details {
                Some(details) => merge_described_options(indexed, details),
                None => indexed,
            };

            let mut option_deps = Vec::new();
            {
                let mut dep_stmt = conn.prepare(
                    "SELECT DISTINCT dep_origin FROM option_deps
                     WHERE port_origin = ?1 AND option_name = ?2 ORDER BY dep_origin",
                )?;
                for (opt_name, ..) in &options {
                    let deps: Vec<String> = dep_stmt
                        .query_map(params![origin, opt_name], |row| row.get::<_, String>(0))?
                        .filter_map(|r| r.ok())
                        .collect();
                    for dep in deps {
                        if seen.insert(dep.clone()) {
                            queue.push_back(dep.clone());
                        }
                        option_deps.push((opt_name.clone(), dep));
                    }
                }
            }

            let requires: Vec<String> = {
                let mut req_stmt = conn.prepare(
                    "SELECT DISTINCT dep_origin FROM port_deps WHERE port_origin = ?1 ORDER BY dep_origin",
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

            self.record_pkg_name(conn, &origin, details.as_ref())?;

            collected.insert(
                origin,
                Collected {
                    options,
                    option_deps,
                    requires,
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
            for (opt_name, default_state, description, group_type, group_name) in entry.options {
                let initial_enabled = sys_opts.get_state(origin, &opt_name, default_state);
                let dep_origins: Vec<String> = entry
                    .option_deps
                    .iter()
                    .filter(|(name, _)| name == &opt_name)
                    .map(|(_, dep)| dep.clone())
                    .collect();
                // Every dependency was queued during the walk, so it is in
                // by_origin, which was fully populated before this loop.
                let dep_idx: Vec<usize> = dep_origins
                    .iter()
                    .filter_map(|d| self.by_origin.get(d).copied())
                    .collect();

                let opt_id = self.option_nodes.len();
                self.option_nodes.push(OptionNode {
                    id: opt_id,
                    port_origin: origin.clone(),
                    name: opt_name,
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
                    dep_idx,
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

    fn record_pkg_name(
        &mut self,
        conn: &Connection,
        origin: &str,
        details: Option<&PortDetails>,
    ) -> Result<()> {
        if self.pkg_names.contains_key(origin) {
            return Ok(());
        }

        // The tree's own PKGNAME whenever it has been read, since the indexer
        // cannot reconstruct one
        if let Some(details) = details {
            self.pkg_names
                .insert(origin.to_string(), details.pkgname.clone());
            return Ok(());
        }

        let pkg_name = conn
            .query_row(
                "SELECT name, version FROM ports WHERE origin = ?1",
                params![origin],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok()
            .map(|(name, version)| {
                // The indexer only reads PORTVERSION, so `version` is its
                // "latest" placeholder — or empty, where PORTVERSION expands
                // through a variable it could not resolve — on the many ports
                // that set DISTVERSION instead. A bare name is more honest than
                // a made-up or half-formed version.
                let version = version.trim();
                if version.is_empty() || version == "latest" {
                    name.trim().to_string()
                } else {
                    format!("{}-{}", name.trim(), version)
                }
            });

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

    /// True when something currently pulls this port in.
    pub fn is_live(&self, origin: &str) -> bool {
        self.port_index(origin)
            .map(|i| self.live[i])
            .unwrap_or(false)
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
                if !opt.enabled {
                    continue;
                }
                for &dep in &opt.dep_idx {
                    if !live[dep] {
                        live[dep] = true;
                        queue.push_back(dep);
                    }
                }
            }
        }

        self.live = live;
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

        for port_id in 0..self.ports.len() {
            self.flatten_port(port_id, &mut rows);
        }

        // Apply active search filter
        if !self.search_query.is_empty() {
            let query = self.search_query.to_lowercase();
            rows.retain(|row| match &row.kind {
                RowKind::Port { origin, .. } => origin.to_lowercase().contains(&query),
                RowKind::Option {
                    name,
                    description,
                    group_name,
                    ..
                } => {
                    name.to_lowercase().contains(&query)
                        || description.to_lowercase().contains(&query)
                        || group_name.to_lowercase().contains(&query)
                }
                RowKind::DependsOn { origin, .. }
                | RowKind::RequiresEntry { origin }
                | RowKind::RequiredByEntry { origin, .. } => origin.to_lowercase().contains(&query),
                RowKind::SectionHeader { .. } | RowKind::Info { .. } => true,
            });
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
            },
            is_expanded: port.is_expanded,
            has_children,
            node_id: NodeId::Port(port_id),
        });

        if !port.is_expanded {
            return;
        }

        if !has_children {
            rows.push(VisibleRow {
                depth: 1,
                kind: RowKind::Info {
                    message: "(No options defined for this port)".to_string(),
                },
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
