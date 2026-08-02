use anyhow::{Context, Result};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct IndexerStats {
    pub ports_indexed: usize,
    pub options_indexed: usize,
    pub option_deps_indexed: usize,
    pub port_deps_indexed: usize,
}

/// The dependency classes making up `_UNIFIED_DEPENDS` in bsd.port.mk, which is
/// what `make config-recursive` — and so `poudriere options` — walks. Ordered
/// most- to least-specific so the `_DEPENDS` suffixes cannot shadow each other.
const UNCONDITIONAL_DEP_VARS: [&str; 8] = [
    "EXTRACT_DEPENDS",
    "PATCH_DEPENDS",
    "FETCH_DEPENDS",
    "BUILD_DEPENDS",
    "LIB_DEPENDS",
    "RUN_DEPENDS",
    "TEST_DEPENDS",
    "PKG_DEPENDS",
];

#[derive(Debug)]
struct ExtractedOption {
    name: String,
    group_type: String,
    group_name: String,
}

#[derive(Debug)]
struct ParsedOptionDep {
    opt_name: String,
    dep_origin: String,
}

#[derive(Debug)]
struct ParsedPort {
    origin: String,
    name: String,
    version: String,
    comment: String,
    options: Vec<(ExtractedOption, bool, String)>, // (opt, default_state, description)
    deps: Vec<ParsedOptionDep>,
    port_deps: Vec<(String, String)>, // (dep_origin, dep_type)
}

fn preprocess_makefile(content: &str) -> String {
    let mut result = String::new();
    for line in content.lines() {
        let line_without_comment = if let Some(idx) = line.find('#') {
            &line[..idx]
        } else {
            line
        };

        let trimmed = line_without_comment.trim_end();
        if trimmed.ends_with('\\') {
            result.push_str(&trimmed[..trimmed.len() - 1]);
            result.push(' ');
        } else {
            result.push_str(trimmed);
            result.push('\n');
        }
    }
    result
}

fn expand_vars(text: &str, vars: &HashMap<String, String>) -> String {
    let re_var_ref =
        Regex::new(r"\$\{([A-Za-z0-9_]+)\}|\$([A-Za-z0-9_]+)|\$\(([A-Za-z0-9_]+)\)").unwrap();
    let mut expanded = text.to_string();

    for _ in 0..5 {
        let mut changed = false;
        let next = re_var_ref
            .replace_all(&expanded, |caps: &regex::Captures| {
                let var_name = caps
                    .get(1)
                    .or_else(|| caps.get(2))
                    .or_else(|| caps.get(3))
                    .map(|m| m.as_str())
                    .unwrap_or("");

                if let Some(val) = vars.get(var_name) {
                    changed = true;
                    val.clone()
                } else {
                    "".to_string()
                }
            })
            .to_string();

        expanded = next;
        if !changed {
            break;
        }
    }
    expanded
}

fn is_valid_option_name(opt: &str) -> bool {
    !opt.is_empty()
        && opt
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        && opt.chars().any(|ch| ch.is_ascii_uppercase())
}

fn parse_port_dir(origin: &str, port_path: &Path) -> Option<ParsedPort> {
    let mut makefile_paths = Vec::new();
    if let Ok(files) = fs::read_dir(port_path) {
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().to_string();
            if name == "Makefile" || name.starts_with("Makefile.") {
                makefile_paths.push(file.path());
            }
        }
    }

    if makefile_paths.is_empty() {
        return None;
    }

    makefile_paths.sort();

    let mut raw_content = String::new();
    for path in makefile_paths {
        if let Ok(c) = fs::read_to_string(&path) {
            raw_content.push_str(&c);
            raw_content.push('\n');
        }
    }

    let content = preprocess_makefile(&raw_content);

    let re_portname = Regex::new(r"(?m)^\s*PORTNAME\s*[:\+\?]?=\s*(.+)$").ok()?;
    let re_version = Regex::new(r"(?m)^\s*PORTVERSION\s*[:\+\?]?=\s*(.+)$").ok()?;
    let re_comment = Regex::new(r"(?m)^\s*COMMENT\s*[:\+\?]?=\s*(.+)$").ok()?;
    let re_options_group = Regex::new(
        r"(?m)^\s*OPTIONS_(DEFINE|SINGLE_[A-Za-z0-9_]+|RADIO_[A-Za-z0-9_]+|MULTI_[A-Za-z0-9_]+|GROUP_[A-Za-z0-9_]+)(?:_[A-Za-z0-9_]+)?\s*[:\+\?]?=\s*(.+)$",
    ).ok()?;
    let re_defaults = Regex::new(r"(?m)^\s*OPTIONS_DEFAULT(?:_\w+)?\s*[:\+\?]?=\s*(.+)$").ok()?;
    let re_opt_desc = Regex::new(r"(?m)^\s*([A-Za-z0-9_]+)_DESC\s*[:\+\?]?=\s*(.+)$").ok()?;
    let re_opt_deps = Regex::new(
        r"(?m)^\s*([A-Z0-9_]+)_(?:BUILD_DEPENDS|RUN_DEPENDS|LIB_DEPENDS|USE|USES)\s*[:\+\?]?=\s*(.+)$",
    ).ok()?;
    // Anchored at the start of the line, so an option-conditional
    // `FOO_BUILD_DEPENDS=` cannot be mistaken for an unconditional one.
    let re_port_deps = Regex::new(&format!(
        r"(?m)^\s*({})\s*[:\+\?]?=\s*(.+)$",
        UNCONDITIONAL_DEP_VARS.join("|")
    ))
    .ok()?;
    let re_origin_extract = Regex::new(r"([a-zA-Z0-9_\-]+/[a-zA-Z0-9_\-]+)").ok()?;
    let re_var_assign = Regex::new(r"(?m)^\s*([A-Za-z0-9_]+)\s*([:\+\?]?)=\s*(.+)$").ok()?;

    let port_folder = port_path.file_name()?.to_str()?;

    // Assignment operators are honoured rather than everything being treated as
    // `+=`. A port that sets the same variable under several conditionals — a
    // `PKGNAMESUFFIX` per flavour, say — would otherwise accumulate every branch
    // at once, and expansions using it come out as nonsense like
    // `subversion-lts-lts-lts-lts-lts`.
    //
    // Only one branch of a conditional is ever really taken; reading the file
    // linearly, last-one-wins is the closest approximation available without
    // evaluating the Makefile.
    let mut vars: HashMap<String, String> = HashMap::new();
    for cap in re_var_assign.captures_iter(&content) {
        if let (Some(k), Some(op), Some(v)) = (cap.get(1), cap.get(2), cap.get(3)) {
            let key = k.as_str().trim().to_string();
            let val = v.as_str().trim().to_string();

            match op.as_str() {
                "+" => vars
                    .entry(key)
                    .and_modify(|e| {
                        e.push(' ');
                        e.push_str(&val);
                    })
                    .or_insert(val),
                // `?=` only applies when the variable has no value yet
                "?" => vars.entry(key).or_insert(val),
                _ => {
                    vars.insert(key.clone(), val);
                    vars.get_mut(&key).expect("just inserted")
                }
            };
        }
    }

    let name = re_portname
        .captures(&content)
        .and_then(|c| c.get(1))
        .map(|m| expand_vars(m.as_str().trim(), &vars))
        .unwrap_or_else(|| port_folder.to_string());

    let version = re_version
        .captures(&content)
        .and_then(|c| c.get(1))
        .map(|m| expand_vars(m.as_str().trim(), &vars))
        .unwrap_or_else(|| "latest".to_string());

    let comment = re_comment
        .captures(&content)
        .and_then(|c| c.get(1))
        .map(|m| expand_vars(m.as_str().trim(), &vars))
        .unwrap_or_default();

    let mut defined_opts: Vec<ExtractedOption> = Vec::new();
    for cap in re_options_group.captures_iter(&content) {
        if let (Some(group_cap), Some(val_cap)) = (cap.get(1), cap.get(2)) {
            let group_raw = group_cap.as_str();
            let (group_type, group_name) = if group_raw == "DEFINE" {
                ("DEFINE".to_string(), "".to_string())
            } else if let Some(rest) = group_raw.strip_prefix("SINGLE_") {
                ("SINGLE".to_string(), rest.to_string())
            } else if let Some(rest) = group_raw.strip_prefix("RADIO_") {
                ("RADIO".to_string(), rest.to_string())
            } else if let Some(rest) = group_raw.strip_prefix("MULTI_") {
                ("MULTI".to_string(), rest.to_string())
            } else if let Some(rest) = group_raw.strip_prefix("GROUP_") {
                ("GROUP".to_string(), rest.to_string())
            } else {
                ("DEFINE".to_string(), "".to_string())
            };

            let expanded = expand_vars(val_cap.as_str(), &vars);
            for opt in expanded.split_whitespace() {
                if is_valid_option_name(opt) && !defined_opts.iter().any(|o| o.name == opt) {
                    defined_opts.push(ExtractedOption {
                        name: opt.to_string(),
                        group_type: group_type.clone(),
                        group_name: group_name.clone(),
                    });
                }
            }
        }
    }

    let mut default_opts = Vec::new();
    for cap in re_defaults.captures_iter(&content) {
        if let Some(m) = cap.get(1) {
            let expanded = expand_vars(m.as_str(), &vars);
            for opt in expanded.split_whitespace() {
                if is_valid_option_name(opt) {
                    default_opts.push(opt.to_string());
                }
            }
        }
    }

    let mut opt_descs = HashMap::new();
    for cap in re_opt_desc.captures_iter(&content) {
        if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
            let key = k.as_str().trim().to_string();
            if is_valid_option_name(&key) {
                let val = expand_vars(v.as_str().trim(), &vars);
                opt_descs.insert(key, val);
            }
        }
    }

    let mut options = Vec::new();
    for opt in defined_opts {
        let default_state = default_opts.contains(&opt.name);
        let desc = opt_descs.get(&opt.name).cloned().unwrap_or_default();
        options.push((opt, default_state, desc));
    }

    let mut deps = Vec::new();
    for cap in re_opt_deps.captures_iter(&content) {
        if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
            let opt_name = k.as_str().trim();
            if options.iter().any(|(o, _, _)| o.name == opt_name) {
                let dep_val = expand_vars(v.as_str(), &vars);
                for dep_match in re_origin_extract.find_iter(&dep_val) {
                    let dep_origin = dep_match.as_str().to_string();
                    if dep_origin != origin {
                        deps.push(ParsedOptionDep {
                            opt_name: opt_name.to_string(),
                            dep_origin,
                        });
                    }
                }
            }
        }
    }

    // Unconditional dependencies. A port can be named by several classes at
    // once (LIB_DEPENDS and RUN_DEPENDS, typically); the first class wins, so
    // that each edge appears in the tree exactly once.
    let mut port_deps: Vec<(String, String)> = Vec::new();
    for cap in re_port_deps.captures_iter(&content) {
        if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
            let dep_type = k.as_str().trim_end_matches("_DEPENDS");
            let dep_val = expand_vars(v.as_str(), &vars);
            for dep_match in re_origin_extract.find_iter(&dep_val) {
                let dep_origin = dep_match.as_str();
                if dep_origin != origin && !port_deps.iter().any(|(d, _)| d == dep_origin) {
                    port_deps.push((dep_origin.to_string(), dep_type.to_string()));
                }
            }
        }
    }

    Some(ParsedPort {
        origin: origin.to_string(),
        name,
        version,
        comment,
        options,
        deps,
        port_deps,
    })
}

pub fn index_ports_dir(conn: &mut Connection, ports_dir: &Path) -> Result<IndexerStats> {
    let mut stats = IndexerStats {
        ports_indexed: 0,
        options_indexed: 0,
        option_deps_indexed: 0,
        port_deps_indexed: 0,
    };

    // 1. Collect all port directory targets
    let mut port_targets: Vec<(String, PathBuf)> = Vec::new();
    let entries = fs::read_dir(ports_dir)
        .with_context(|| format!("Failed to read ports directory at {:?}", ports_dir))?;

    for entry in entries.flatten() {
        let category_path = entry.path();
        if !category_path.is_dir() {
            continue;
        }

        let category_name = match category_path.file_name().and_then(|s| s.to_str()) {
            Some(name) if !name.starts_with('.') && name != "Mk" && name != "Templates" => name,
            _ => continue,
        };

        if let Ok(port_entries) = fs::read_dir(&category_path) {
            for port_entry in port_entries.flatten() {
                let port_path = port_entry.path();
                if port_path.is_dir() {
                    if let Some(port_folder) = port_path.file_name().and_then(|s| s.to_str()) {
                        let origin = format!("{}/{}", category_name, port_folder);
                        port_targets.push((origin, port_path));
                    }
                }
            }
        }
    }

    // 2. Parse Makefiles concurrently across all CPU cores with Rayon
    let parsed_ports: Vec<ParsedPort> = port_targets
        .par_iter()
        .filter_map(|(origin, path)| parse_port_dir(origin, path))
        .collect();

    // 3. Commit parsed results inside a single SQLite transaction
    let tx = conn.transaction()?;

    for port in parsed_ports {
        tx.execute(
            "INSERT OR REPLACE INTO ports (origin, name, version, comment) VALUES (?1, ?2, ?3, ?4)",
            params![port.origin, port.name, port.version, port.comment],
        )?;
        stats.ports_indexed += 1;

        for (opt, default_state, desc) in port.options {
            tx.execute(
                "INSERT OR REPLACE INTO options (port_origin, option_name, default_state, description, group_type, group_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![port.origin, opt.name, if default_state { 1 } else { 0 }, desc, opt.group_type, opt.group_name],
            )?;
            stats.options_indexed += 1;
        }

        for dep in port.deps {
            tx.execute(
                "INSERT OR REPLACE INTO option_deps (port_origin, option_name, dep_origin, dep_type)
                 VALUES (?1, ?2, ?3, 'RUN')",
                params![port.origin, dep.opt_name, dep.dep_origin],
            )?;
            stats.option_deps_indexed += 1;
        }

        for (dep_origin, dep_type) in port.port_deps {
            tx.execute(
                "INSERT OR REPLACE INTO port_deps (port_origin, dep_origin, dep_type)
                 VALUES (?1, ?2, ?3)",
                params![port.origin, dep_origin, dep_type],
            )?;
            stats.port_deps_indexed += 1;
        }
    }

    // Depends lines name their targets in forms the origin regex cannot always
    // tell apart from a plain path (`${LOCALBASE}/bin/foo` collapses to
    // `bin/foo` once an unresolved variable expands away). Anything that is not
    // a port we actually indexed cannot be configured, so drop it rather than
    // hang an empty node off the tree for it.
    let pruned_opt = tx.execute(
        "DELETE FROM option_deps WHERE dep_origin NOT IN (SELECT origin FROM ports)",
        [],
    )?;
    let pruned_port = tx.execute(
        "DELETE FROM port_deps WHERE dep_origin NOT IN (SELECT origin FROM ports)",
        [],
    )?;
    stats.option_deps_indexed -= pruned_opt.min(stats.option_deps_indexed);
    stats.port_deps_indexed -= pruned_port.min(stats.port_deps_indexed);

    tx.commit()?;
    Ok(stats)
}
