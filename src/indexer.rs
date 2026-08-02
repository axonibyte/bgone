use anyhow::{Context, Result};
use rayon::prelude::*;
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
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
        if let Some(continued) = trimmed.strip_suffix('\\') {
            result.push_str(continued);
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

/// Collects every variable assignment in a preprocessed Makefile.
///
/// Assignment operators are honoured rather than everything being treated as
/// `+=`. A port that sets the same variable under several conditionals — a
/// `PKGNAMESUFFIX` per flavour, say — would otherwise accumulate every branch at
/// once, and expansions using it come out as nonsense like
/// `subversion-lts-lts-lts-lts-lts`.
///
/// Only one branch of a conditional is ever really taken; reading the file
/// linearly, last-one-wins is the closest approximation available without
/// evaluating the Makefile.
fn collect_vars(content: &str) -> HashMap<String, String> {
    let Ok(re_var_assign) = Regex::new(r"(?m)^\s*([A-Za-z0-9_]+)\s*([:\+\?]?)=\s*(.+)$") else {
        return HashMap::new();
    };

    let mut vars: HashMap<String, String> = HashMap::new();
    for cap in re_var_assign.captures_iter(content) {
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
    vars
}

/// Reads every `Makefile*` in one directory, in name order, into one string.
fn read_makefiles(dir: &Path) -> Option<String> {
    let mut paths = Vec::new();
    if let Ok(files) = fs::read_dir(dir) {
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().to_string();
            if name == "Makefile" || name.starts_with("Makefile.") {
                paths.push(file.path());
            }
        }
    }

    if paths.is_empty() {
        return None;
    }
    paths.sort();

    let mut raw = String::new();
    for path in paths {
        if let Ok(c) = fs::read_to_string(&path) {
            raw.push_str(&c);
            raw.push('\n');
        }
    }
    Some(raw)
}

/// Resolves a `MASTERDIR` value to the directory it names.
///
/// Every one of the 1,128 ports in a current tree that sets `MASTERDIR` writes
/// it against `${.CURDIR}`, in one of three shapes:
///
/// ```text
/// ${.CURDIR}/../nginx                 1024
/// ${.CURDIR:H:H}/multimedia/mplayer     96
/// ${.CURDIR:H}/postfixadmin33            8
/// ```
///
/// `:H` is bmake's "head" modifier — a `dirname` — so each one climbs a level
/// before the rest of the path is appended. Anything else is refused rather
/// than guessed at; a wrong directory would attribute one port's options to
/// another, which is worse than the missing options this exists to fix.
///
/// The result must be a directory inside `tree_root` and must not be the port
/// itself, so a malformed or hostile value cannot walk out of the ports tree.
fn resolve_masterdir(value: &str, port_path: &Path, tree_root: &Path) -> Option<PathBuf> {
    let rest = value.trim();
    let inner = rest.strip_prefix("${.CURDIR")?;
    let (modifiers, tail) = inner.split_once('}')?;

    let mut base = port_path.to_path_buf();
    let mut remaining = modifiers;
    while let Some(next) = remaining.strip_prefix(":H") {
        base = base.parent()?.to_path_buf();
        remaining = next;
    }
    if !remaining.is_empty() {
        return None; // a modifier other than :H — refuse rather than guess
    }

    let joined = base.join(tail.trim_start_matches('/'));
    // Canonicalised on both sides: comparing a resolved path against an
    // unresolved one would let a symlinked tree defeat both guards below.
    let resolved = fs::canonicalize(joined).ok()?;
    let root = fs::canonicalize(tree_root).ok()?;
    let here = fs::canonicalize(port_path).ok()?;

    if !resolved.is_dir() || !resolved.starts_with(&root) || resolved == here {
        return None;
    }
    Some(resolved)
}

/// A chunk of Makefile text, paired with the directory it was read from.
///
/// The directory matters because a relative `.include "options"` resolves
/// against the file that wrote it — which stops being the port itself once
/// `MASTERDIR` has folded a master's Makefile into the same parse.
type Chunk = (String, PathBuf);

fn joined(chunks: &[Chunk]) -> String {
    chunks
        .iter()
        .map(|(text, _)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// How far a chain of slave ports is followed.
///
/// A master can itself be a slave — 10 ports in a current tree are — so one hop
/// is not enough. Nothing observed goes beyond two, and the visited set already
/// makes a cycle terminate; this is only a backstop against a pathological tree.
const MAX_MASTER_DEPTH: usize = 4;

/// How far a chain of `.include` directives is followed. 49 included files in a
/// current tree include something themselves; none go deep.
const MAX_INCLUDE_DEPTH: usize = 4;

/// Folds in the master's Makefiles, and its master's, and so on.
///
/// A slave port's own Makefile is little more than `MASTERDIR` and an
/// `.include`; everything the configurator needs — the option list, the
/// descriptions, the `SINGLE`/`MULTI`/`RADIO` grouping — lives in the master.
///
/// Appended *after* the slave, mirroring the trailing `.include` slaves actually
/// use, so the last-one-wins rule in the variable pass resolves conflicts the
/// way bmake would.
fn fold_masters(chunks: &mut Vec<Chunk>, tree_root: &Path) {
    let Ok(re_masterdir) = Regex::new(r"(?m)^\s*MASTERDIR\s*[:\+\?]?=\s*(.+)$") else {
        return;
    };

    let mut visited: Vec<PathBuf> = chunks.iter().map(|(_, dir)| dir.clone()).collect();

    for _ in 0..MAX_MASTER_DEPTH {
        // Only the newest chunk is searched, so each round sees the master's own
        // MASTERDIR rather than re-resolving the one just followed.
        let Some((text, dir)) = chunks.last().cloned() else {
            return;
        };
        let Some(value) = re_masterdir
            .captures_iter(&text)
            .last()
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
        else {
            return;
        };

        // 46 ports build the path out of another variable —
        // `${.CURDIR}/../${PORTNAME}-server`, `${WANT_PGSQL_VER}` — so the value
        // has to be expanded before it names a directory. `${.CURDIR}` itself
        // survives untouched, since `expand_vars` only matches names made of
        // word characters and cannot see a leading dot.
        let vars = collect_vars(&preprocess_makefile(&joined(chunks)));
        let expanded = expand_vars(&value, &vars);

        let Some(master) = resolve_masterdir(&expanded, &dir, tree_root) else {
            return;
        };
        if visited.contains(&master) {
            return;
        }
        let Some(master_text) = read_makefiles(&master) else {
            return;
        };

        visited.push(master.clone());
        chunks.push((master_text, master));
    }
}

/// Resolves one quoted `.include` value to the file it names.
///
/// The shapes that occur, once `${MASTERDIR}` is excluded as already folded:
/// `${.CURDIR}`-rooted paths with optional `:H` modifiers (730), a bare file
/// name or relative path resolved against the including file's own directory
/// (138), and `${PORTSDIR}` (26). Fifteen others name a variable this parse
/// cannot know — `${USESDIR}`, `${.PARSEDIR}` — and are refused.
///
/// Angle-bracket includes are never considered: those are the framework's own
/// `bsd.port.mk` and friends, which live outside the tree and describe how to
/// build rather than what to build.
fn resolve_include(
    value: &str,
    from_dir: &Path,
    port_path: &Path,
    tree_root: &Path,
    vars: &HashMap<String, String>,
) -> Option<PathBuf> {
    let raw = value.trim();

    // Prefixes are matched before expansion: `expand_vars` turns an unknown name
    // into the empty string, which would silently reduce `${PORTSDIR}/x` to the
    // absolute `/x` instead of failing.
    let joined_path = if let Some(inner) = raw.strip_prefix("${.CURDIR") {
        let (modifiers, tail) = inner.split_once('}')?;
        let mut base = port_path.to_path_buf();
        let mut remaining = modifiers;
        while let Some(next) = remaining.strip_prefix(":H") {
            base = base.parent()?.to_path_buf();
            remaining = next;
        }
        if !remaining.is_empty() {
            return None;
        }
        base.join(expand_vars(tail.trim_start_matches('/'), vars))
    } else if let Some(tail) = raw.strip_prefix("${PORTSDIR}") {
        tree_root.join(expand_vars(tail.trim_start_matches('/'), vars))
    } else if raw.starts_with('$') || raw.starts_with('/') {
        // A variable this parse cannot resolve, or an absolute path out of the
        // tree. Refused rather than guessed at.
        return None;
    } else {
        from_dir.join(expand_vars(raw, vars))
    };

    let resolved = fs::canonicalize(joined_path).ok()?;
    let root = fs::canonicalize(tree_root).ok()?;
    if !resolved.is_file() || !resolved.starts_with(&root) {
        return None;
    }
    Some(resolved)
}

/// Folds in files pulled in by a quoted `.include`.
///
/// `mail/exim` keeps its whole option list in a file called `options` and pulls
/// it in with `.include "options"`; reading only `Makefile*` left it and its six
/// slaves with nothing. Runs after [`fold_masters`] so a master's own includes
/// are followed, resolved against the master's directory rather than the slave's.
fn fold_includes(chunks: &mut Vec<Chunk>, port_path: &Path, tree_root: &Path) {
    let Ok(re_include) = Regex::new(r#"(?m)^\s*\.\s*include\s+"([^"]+)""#) else {
        return;
    };
    // Most ports include nothing quoted; this keeps them clear of the
    // preprocess-and-collect below, which is the expensive part.
    if !chunks.iter().any(|(text, _)| re_include.is_match(text)) {
        return;
    }

    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut frontier: Vec<Chunk> = chunks.clone();

    for _ in 0..MAX_INCLUDE_DEPTH {
        let vars = collect_vars(&preprocess_makefile(&joined(chunks)));
        let mut next: Vec<Chunk> = Vec::new();

        for (text, dir) in &frontier {
            for cap in re_include.captures_iter(text) {
                let Some(value) = cap.get(1).map(|m| m.as_str()) else {
                    continue;
                };
                // `fold_masters` already brought the master in; following this
                // as well would parse it twice.
                if value.contains("MASTERDIR") {
                    continue;
                }
                let Some(path) = resolve_include(value, dir, port_path, tree_root, &vars) else {
                    continue;
                };
                if !visited.insert(path.clone()) {
                    continue;
                }
                let Ok(body) = fs::read_to_string(&path) else {
                    continue;
                };
                let base = path.parent().unwrap_or(port_path).to_path_buf();
                next.push((body, base));
            }
        }

        if next.is_empty() {
            return;
        }
        chunks.extend(next.iter().cloned());
        frontier = next;
    }
}

fn parse_port_dir(origin: &str, port_path: &Path) -> Option<ParsedPort> {
    let mut chunks: Vec<Chunk> = vec![(read_makefiles(port_path)?, port_path.to_path_buf())];

    // `tree_root` is derived from the origin rather than passed down: an origin
    // is always `category/port`, so the root is two levels up from the port.
    if let Some(tree_root) = port_path.parent().and_then(|p| p.parent()) {
        fold_masters(&mut chunks, tree_root);
        fold_includes(&mut chunks, port_path, tree_root);
    }

    let raw_content = joined(&chunks);

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

    let port_folder = port_path.file_name()?.to_str()?;

    let vars = collect_vars(&content);

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

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------ preprocess_makefile

    /// A continued line becomes one line, so a regex anchored at `^` sees the
    /// whole value rather than just the first fragment.
    #[test]
    fn a_backslash_continuation_is_folded_into_one_line() {
        let out = preprocess_makefile("OPTIONS_DEFINE= ALPHA \\\n\tBETA GAMMA\nPORTNAME= x\n");
        let first = out.lines().next().unwrap();
        assert!(first.contains("ALPHA"), "got {first:?}");
        assert!(
            first.contains("BETA") && first.contains("GAMMA"),
            "the continuation should be on the same line: {first:?}"
        );
    }

    #[test]
    fn a_trailing_comment_is_stripped() {
        let out = preprocess_makefile("OPTIONS_DEFINE= SSL # enable TLS\n");
        assert_eq!(out, "OPTIONS_DEFINE= SSL\n");
    }

    /// Comment stripping cuts at the *first* `#` anywhere on the line, which is
    /// not what bmake does — there, a `#` only opens a comment at the start of a
    /// line or after whitespace. A URL fragment or a `:M#` modifier is therefore
    /// truncated.
    ///
    /// Pinned rather than fixed because nothing downstream reads the affected
    /// variables: option names, descriptions and dependency origins never
    /// legitimately contain `#`. It is here so the next person to look at a
    /// mangled `MASTER_SITES` finds the cause immediately.
    #[test]
    fn a_hash_inside_a_value_truncates_it_too() {
        let out = preprocess_makefile("MASTER_SITES= https://example.invalid/x#frag\n");
        assert_eq!(out, "MASTER_SITES= https://example.invalid/x\n");
    }

    // ------------------------------------------------------------- expand_vars

    fn vars_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn all_three_reference_forms_expand() {
        let vars = vars_of(&[("PORTNAME", "nginx")]);
        assert_eq!(expand_vars("${PORTNAME}", &vars), "nginx");
        assert_eq!(expand_vars("$(PORTNAME)", &vars), "nginx");
        assert_eq!(expand_vars("$PORTNAME", &vars), "nginx");
    }

    /// bmake reads a bare `$` as taking exactly one character, so `$FOO` there
    /// means `${F}OO`. This takes the whole identifier instead.
    ///
    /// A deviation, but a safe one in this direction: ports write `${FOO}` and
    /// the single-character form is vanishingly rare outside `$$` and shell
    /// lines, which are not read here.
    #[test]
    fn a_bare_dollar_takes_the_whole_name_unlike_bmake() {
        let vars = vars_of(&[("F", "one"), ("FOO", "two")]);
        assert_eq!(expand_vars("$FOO", &vars), "two");
    }

    #[test]
    fn references_nest() {
        let vars = vars_of(&[("A", "${B}"), ("B", "leaf")]);
        assert_eq!(expand_vars("${A}", &vars), "leaf");
    }

    /// An unknown variable expands to nothing rather than being left alone.
    ///
    /// This is what drops `.for`-generated options: the loop variable is never
    /// bound, so `OPTIONS_DEFINE+=${O}` yields an empty name that
    /// [`is_valid_option_name`] then rejects.
    #[test]
    fn an_unknown_reference_expands_to_nothing() {
        let vars = vars_of(&[("KNOWN", "yes")]);
        assert_eq!(expand_vars("${UNSET}", &vars), "");
        assert_eq!(expand_vars("a${UNSET}b", &vars), "ab");
        assert!(!is_valid_option_name(&expand_vars("${O}", &vars)));
    }

    /// Expansion gives up after five rounds, leaving the reference in place.
    ///
    /// The cap is what stops a self-referential `A=${A}` spinning; the cost is
    /// that a chain deeper than five resolves partly, and a leftover `${...}`
    /// is then rejected as an option name rather than being silently accepted.
    #[test]
    fn expansion_gives_up_after_five_rounds() {
        let deep = vars_of(&[
            ("A", "${B}"),
            ("B", "${C}"),
            ("C", "${D}"),
            ("D", "${E}"),
            ("E", "${F}"),
            ("F", "end"),
        ]);
        assert_eq!(expand_vars("${A}", &deep), "${F}");

        let cyclic = vars_of(&[("LOOP", "${LOOP}")]);
        assert_eq!(expand_vars("${LOOP}", &cyclic), "${LOOP}");
    }

    // ------------------------------------------------------ is_valid_option_name

    #[test]
    fn option_names_are_uppercase_digits_and_underscores() {
        for ok in ["SSL", "HTTP2", "GZIP_DEF", "X"] {
            assert!(is_valid_option_name(ok), "{ok} should be accepted");
        }
        for bad in [
            "",        // an unexpanded loop variable, post-expansion
            "ssl",     // lowercase
            "123",     // digits alone, no letter to make it a name
            "SSL-X",   // punctuation
            "${O}",    // an expansion that did not resolve
            "SSL DEF", // two names that failed to be split
        ] {
            assert!(!is_valid_option_name(bad), "{bad:?} should be rejected");
        }
    }
}
