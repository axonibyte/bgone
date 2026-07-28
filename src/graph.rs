use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Expand,
    Collapse,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeId {
    Port(usize),
    Option(usize),
    Info,
}

#[derive(Debug, Clone)]
pub enum RowKind {
    Port {
        origin: String,
    },
    Option {
        name: String,
        description: String,
        enabled: bool,
        group_type: String,
        group_name: String,
    },
    Info {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct VisibleRow {
    pub depth: usize,
    pub kind: RowKind,
    pub is_expanded: bool,
    pub has_children: bool,
    pub node_id: NodeId,
}

#[derive(Debug, Clone)]
pub struct OptionNode {
    pub _id: usize,
    pub _port_origin: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub group_type: String,
    pub group_name: String,
    pub is_expanded: bool,
    pub last_single_seq: u64,
    pub subtree_seq: u64,
    pub subtree_mode: Mode,
    pub parent_port: usize,
    pub child_ports: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct PortNode {
    pub id: usize,
    pub origin: String,
    pub depth: usize,
    pub is_expanded: bool,
    pub last_single_seq: u64,
    pub subtree_seq: u64,
    pub subtree_mode: Mode,
    pub parent_option: Option<usize>,
    pub options: Vec<usize>,
}

pub struct DependencyGraph {
    pub root_origin: String,
    pub port_nodes: Vec<PortNode>,
    pub option_nodes: Vec<OptionNode>,
    pub root_port_id: usize,
    pub visible_rows: Vec<VisibleRow>,

    pub global_mode: Mode,
    pub last_global_seq: u64,
    pub current_seq: u64,
}

impl DependencyGraph {
    pub fn load_from_db(conn: &Connection, root_origin: &str) -> Result<Self> {
        let mut graph = Self {
            root_origin: root_origin.to_string(),
            port_nodes: Vec::new(),
            option_nodes: Vec::new(),
            root_port_id: 0,
            visible_rows: Vec::new(),
            global_mode: Mode::None,
            last_global_seq: 0,
            current_seq: 0,
        };

        let mut visited = HashSet::new();
        graph.root_port_id = graph.load_port_recursive(conn, root_origin, 0, None, &mut visited)?;
        graph.rebuild_visible_rows();

        Ok(graph)
    }

    fn load_port_recursive(
        &mut self,
        conn: &Connection,
        origin: &str,
        depth: usize,
        parent_option: Option<usize>,
        visited: &mut HashSet<String>,
    ) -> Result<usize> {
        let port_id = self.port_nodes.len();
        visited.insert(origin.to_string());

        let port_node = PortNode {
            id: port_id,
            origin: origin.to_string(),
            depth,
            is_expanded: false,
            last_single_seq: 0,
            subtree_seq: 0,
            subtree_mode: Mode::None,
            parent_option,
            options: Vec::new(),
        };
        self.port_nodes.push(port_node);

        let mut stmt = conn.prepare(
            "SELECT option_name, default_state, description, group_type, group_name FROM options WHERE port_origin = ?1",
        )?;
        let rows = stmt.query_map(params![origin], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i32>(1)? == 1,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut option_data = Vec::new();
        for r in rows {
            if let Ok(data) = r {
                option_data.push(data);
            }
        }

        for (opt_name, default_state, description, group_type, group_name) in option_data {
            let opt_id = self.option_nodes.len();
            let opt_node = OptionNode {
                id: opt_id,
                port_origin: origin.to_string(),
                name: opt_name.clone(),
                description,
                enabled: default_state,
                group_type,
                group_name,
                is_expanded: false,
                last_single_seq: 0,
                subtree_seq: 0,
                subtree_mode: Mode::None,
                parent_port: port_id,
                child_ports: Vec::new(),
            };
            self.option_nodes.push(opt_node);
            self.port_nodes[port_id].options.push(opt_id);

            if depth < 4 {
                let mut dep_stmt = conn.prepare(
                    "SELECT DISTINCT dep_origin FROM option_deps WHERE port_origin = ?1 AND option_name = ?2",
                )?;
                let dep_rows =
                    dep_stmt.query_map(params![origin, opt_name], |row| row.get::<_, String>(0))?;

                let mut deps = Vec::new();
                for dr in dep_rows {
                    if let Ok(d) = dr {
                        if !visited.contains(&d) {
                            deps.push(d);
                        }
                    }
                }

                for dep_origin in deps {
                    let child_id = self.load_port_recursive(
                        conn,
                        &dep_origin,
                        depth + 2,
                        Some(opt_id),
                        visited,
                    )?;
                    self.option_nodes[opt_id].child_ports.push(child_id);
                }
            }
        }

        self.port_nodes[port_id].is_expanded = self.get_effective_port_expansion(port_id);
        for &opt_id in &self.port_nodes[port_id].options {
            self.option_nodes[opt_id].is_expanded = self.get_effective_option_expansion(opt_id);
        }

        visited.remove(origin);
        Ok(port_id)
    }

    fn get_ancestor_subtree_info_for_port(&self, port_id: usize) -> (u64, Mode) {
        let mut max_seq = self.port_nodes[port_id].subtree_seq;
        let mut max_mode = self.port_nodes[port_id].subtree_mode;

        let mut curr_opt_id = self.port_nodes[port_id].parent_option;
        while let Some(opt_id) = curr_opt_id {
            let opt = &self.option_nodes[opt_id];
            if opt.subtree_seq > max_seq {
                max_seq = opt.subtree_seq;
                max_mode = opt.subtree_mode;
            }
            let port = &self.port_nodes[opt.parent_port];
            if port.subtree_seq > max_seq {
                max_seq = port.subtree_seq;
                max_mode = port.subtree_mode;
            }
            curr_opt_id = port.parent_option;
        }

        (max_seq, max_mode)
    }

    fn get_ancestor_subtree_info_for_option(&self, opt_id: usize) -> (u64, Mode) {
        let mut max_seq = self.option_nodes[opt_id].subtree_seq;
        let mut max_mode = self.option_nodes[opt_id].subtree_mode;

        let parent_port_id = self.option_nodes[opt_id].parent_port;
        let (port_max_seq, port_max_mode) = self.get_ancestor_subtree_info_for_port(parent_port_id);

        if port_max_seq > max_seq {
            max_seq = port_max_seq;
            max_mode = port_max_mode;
        }

        (max_seq, max_mode)
    }

    pub fn get_effective_port_expansion(&self, port_id: usize) -> bool {
        let port = &self.port_nodes[port_id];
        let (ancestor_seq, ancestor_mode) = self.get_ancestor_subtree_info_for_port(port_id);

        let single_seq = port.last_single_seq;
        let global_seq = self.last_global_seq;

        if single_seq > ancestor_seq && single_seq > global_seq {
            port.is_expanded
        } else if ancestor_seq > global_seq {
            ancestor_mode == Mode::Expand
        } else if global_seq > 0 {
            self.global_mode == Mode::Expand
        } else {
            port.depth == 0
        }
    }

    pub fn get_effective_option_expansion(&self, opt_id: usize) -> bool {
        let opt = &self.option_nodes[opt_id];
        let (ancestor_seq, ancestor_mode) = self.get_ancestor_subtree_info_for_option(opt_id);

        let single_seq = opt.last_single_seq;
        let global_seq = self.last_global_seq;

        if single_seq > ancestor_seq && single_seq > global_seq {
            opt.is_expanded
        } else if ancestor_seq > global_seq {
            ancestor_mode == Mode::Expand
        } else if global_seq > 0 {
            self.global_mode == Mode::Expand
        } else {
            false
        }
    }

    pub fn expand_subtree(&mut self, row_index: usize) {
        if let Some(row) = self.visible_rows.get(row_index) {
            self.current_seq += 1;
            let seq = self.current_seq;

            match row.node_id {
                NodeId::Port(id) => self.apply_port_subtree_mode(id, Mode::Expand, seq),
                NodeId::Option(id) => self.apply_option_subtree_mode(id, Mode::Expand, seq),
                NodeId::Info => {}
            }
        }
        self.rebuild_visible_rows();
    }

    pub fn collapse_subtree(&mut self, row_index: usize) {
        if let Some(row) = self.visible_rows.get(row_index) {
            self.current_seq += 1;
            let seq = self.current_seq;

            match row.node_id {
                NodeId::Port(id) => self.apply_port_subtree_mode(id, Mode::Collapse, seq),
                NodeId::Option(id) => self.apply_option_subtree_mode(id, Mode::Collapse, seq),
                NodeId::Info => {}
            }
        }
        self.rebuild_visible_rows();
    }

    fn apply_port_subtree_mode(&mut self, port_id: usize, mode: Mode, seq: u64) {
        if let Some(p) = self.port_nodes.get_mut(port_id) {
            p.subtree_seq = seq;
            p.subtree_mode = mode;
            p.is_expanded = mode == Mode::Expand;
            let options = p.options.clone();
            for opt_id in options {
                self.apply_option_subtree_mode(opt_id, mode, seq);
            }
        }
    }

    fn apply_option_subtree_mode(&mut self, opt_id: usize, mode: Mode, seq: u64) {
        if let Some(o) = self.option_nodes.get_mut(opt_id) {
            o.subtree_seq = seq;
            o.subtree_mode = mode;
            o.is_expanded = mode == Mode::Expand;
            let child_ports = o.child_ports.clone();
            for child_id in child_ports {
                self.apply_port_subtree_mode(child_id, mode, seq);
            }
        }
    }

    pub fn expand_all(&mut self) {
        self.current_seq += 1;
        self.last_global_seq = self.current_seq;
        self.global_mode = Mode::Expand;

        for port in &mut self.port_nodes {
            port.is_expanded = true;
        }
        for opt in &mut self.option_nodes {
            opt.is_expanded = true;
        }

        self.rebuild_visible_rows();
    }

    pub fn collapse_all(&mut self) {
        self.current_seq += 1;
        self.last_global_seq = self.current_seq;
        self.global_mode = Mode::Collapse;

        for port in &mut self.port_nodes {
            port.is_expanded = false;
        }
        for opt in &mut self.option_nodes {
            opt.is_expanded = false;
        }

        self.rebuild_visible_rows();
    }

    pub fn toggle_expand(&mut self, row_index: usize) {
        if let Some(row) = self.visible_rows.get(row_index) {
            self.current_seq += 1;
            let seq = self.current_seq;

            match row.node_id {
                NodeId::Port(id) => {
                    if let Some(p) = self.port_nodes.get_mut(id) {
                        p.last_single_seq = seq;
                        p.is_expanded = !p.is_expanded;
                    }
                }
                NodeId::Option(id) => {
                    if let Some(o) = self.option_nodes.get_mut(id) {
                        o.last_single_seq = seq;
                        o.is_expanded = !o.is_expanded;
                    }
                }
                NodeId::Info => {}
            }
        }
        self.rebuild_visible_rows();
    }

    pub fn toggle_option(&mut self, row_index: usize) {
        if let Some(row) = self.visible_rows.get(row_index) {
            if let NodeId::Option(id) = row.node_id {
                let (parent_port, group_type, group_name, current_enabled) = {
                    let opt = &self.option_nodes[id];
                    (
                        opt.parent_port,
                        opt.group_type.clone(),
                        opt.group_name.clone(),
                        opt.enabled,
                    )
                };

                let is_radio = group_type == "SINGLE" || group_type == "RADIO";

                if is_radio {
                    // Turn target radio option ON, turn off all sibling options in the same group
                    if !current_enabled {
                        let sibling_ids = self.port_nodes[parent_port].options.clone();
                        for opt_id in sibling_ids {
                            let sibling = &mut self.option_nodes[opt_id];
                            if sibling.group_type == group_type && sibling.group_name == group_name
                            {
                                sibling.enabled = opt_id == id;
                            }
                        }
                    }
                } else if let Some(o) = self.option_nodes.get_mut(id) {
                    o.enabled = !o.enabled;
                }
            }
        }
        self.rebuild_visible_rows();
    }

    pub fn rebuild_visible_rows(&mut self) {
        self.visible_rows.clear();
        if self.port_nodes.is_empty() {
            return;
        }

        let root_id = self.root_port_id;
        self.flatten_port(root_id);
    }

    fn flatten_port(&mut self, port_id: usize) {
        let port = self.port_nodes[port_id].clone();
        let has_children = !port.options.is_empty();

        self.visible_rows.push(VisibleRow {
            depth: port.depth,
            kind: RowKind::Port {
                origin: port.origin.clone(),
            },
            is_expanded: port.is_expanded,
            has_children,
            node_id: NodeId::Port(port_id),
        });

        if port.is_expanded {
            if port.options.is_empty() {
                self.visible_rows.push(VisibleRow {
                    depth: port.depth + 1,
                    kind: RowKind::Info {
                        message: "(No options defined for this port)".to_string(),
                    },
                    is_expanded: false,
                    has_children: false,
                    node_id: NodeId::Info,
                });
            } else {
                for &opt_id in &port.options {
                    self.flatten_option(opt_id, port.depth + 1);
                }
            }
        }
    }

    fn flatten_option(&mut self, opt_id: usize, opt_depth: usize) {
        let opt = self.option_nodes[opt_id].clone();
        let has_children = !opt.child_ports.is_empty();

        self.visible_rows.push(VisibleRow {
            depth: opt_depth,
            kind: RowKind::Option {
                name: opt.name.clone(),
                description: opt.description.clone(),
                enabled: opt.enabled,
                group_type: opt.group_type.clone(),
                group_name: opt.group_name.clone(),
            },
            is_expanded: opt.is_expanded,
            has_children,
            node_id: NodeId::Option(opt_id),
        });

        if opt.is_expanded && opt.enabled {
            for &child_port_id in &opt.child_ports {
                self.flatten_port(child_port_id);
            }
        }
    }
}
