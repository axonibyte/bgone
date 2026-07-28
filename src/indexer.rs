use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct IndexerStats {
    pub ports_indexed: usize,
    pub options_indexed: usize,
    pub option_deps_indexed: usize,
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

#[derive(Debug)]
struct ExtractedOption {
    name: String,
    group_type: String,
    group_name: String,
}

pub fn index_ports_dir(conn: &mut Connection, ports_dir: &Path) -> Result<IndexerStats> {
    let mut stats = IndexerStats {
        ports_indexed: 0,
        options_indexed: 0,
        option_deps_indexed: 0,
    };

    let re_portname = Regex::new(r"(?m)^\s*PORTNAME\s*[:\+\?]?=\s*(.+)$")?;
    let re_version = Regex::new(r"(?m)^\s*PORTVERSION\s*[:\+\?]?=\s*(.+)$")?;
    let re_comment = Regex::new(r"(?m)^\s*COMMENT\s*[:\+\?]?=\s*(.+)$")?;

    let re_options_group = Regex::new(
        r"(?m)^\s*OPTIONS_(DEFINE|SINGLE_[A-Za-z0-9_]+|RADIO_[A-Za-z0-9_]+|MULTI_[A-Za-z0-9_]+|GROUP_[A-Za-z0-9_]+)(?:_[A-Za-z0-9_]+)?\s*[:\+\?]?=\s*(.+)$",
    )?;

    let re_defaults = Regex::new(r"(?m)^\s*OPTIONS_DEFAULT(?:_\w+)?\s*[:\+\?]?=\s*(.+)$")?;

    let re_opt_desc = Regex::new(r"(?m)^\s*([A-Za-z0-9_]+)_DESC\s*[:\+\?]?=\s*(.+)$")?;

    let re_opt_deps = Regex::new(
        r"(?m)^\s*([A-Z0-9_]+)_(?:BUILD_DEPENDS|RUN_DEPENDS|LIB_DEPENDS|USE|USES)\s*[:\+\?]?=\s*(.+)$",
    )?;

    let re_origin_extract = Regex::new(r"([a-zA-Z0-9_\-]+/[a-zA-Z0-9_\-]+)")?;
    let re_var_assign = Regex::new(r"(?m)^\s*([A-Za-z0-9_]+)\s*[:\+\?]?=\s*(.+)$")?;

    let tx = conn.transaction()?;

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

        let port_entries = match fs::read_dir(&category_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for port_entry in port_entries.flatten() {
            let port_path = port_entry.path();
            if !port_path.is_dir() {
                continue;
            }

            let mut makefile_paths = Vec::new();
            if let Ok(files) = fs::read_dir(&port_path) {
                for file in files.flatten() {
                    let name = file.file_name().to_string_lossy().to_string();
                    if name == "Makefile" || name.starts_with("Makefile.") {
                        makefile_paths.push(file.path());
                    }
                }
            }

            if makefile_paths.is_empty() {
                continue;
            }

            makefile_paths.sort();

            let port_folder = match port_path.file_name().and_then(|s| s.to_str()) {
                Some(f) => f,
                None => continue,
            };

            let origin = format!("{}/{}", category_name, port_folder);

            let mut raw_content = String::new();
            for path in makefile_paths {
                if let Ok(c) = fs::read_to_string(&path) {
                    raw_content.push_str(&c);
                    raw_content.push('\n');
                }
            }

            let content = preprocess_makefile(&raw_content);

            let mut vars: HashMap<String, String> = HashMap::new();
            for cap in re_var_assign.captures_iter(&content) {
                if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
                    let key = k.as_str().trim().to_string();
                    let val = v.as_str().trim().to_string();
                    vars.entry(key)
                        .and_modify(|e| {
                            e.push(' ');
                            e.push_str(&val);
                        })
                        .or_insert(val);
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

            tx.execute(
                "INSERT OR REPLACE INTO ports (origin, name, version, comment) VALUES (?1, ?2, ?3, ?4)",
                params![origin, name, version, comment],
            )?;
            stats.ports_indexed += 1;

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
                        if is_valid_option_name(opt) && !defined_opts.iter().any(|o| o.name == opt)
                        {
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

            for opt in &defined_opts {
                let default_state = if default_opts.contains(&opt.name) {
                    1
                } else {
                    0
                };
                let desc = opt_descs.get(&opt.name).cloned().unwrap_or_default();

                tx.execute(
                    "INSERT OR REPLACE INTO options (port_origin, option_name, default_state, description, group_type, group_name)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![origin, opt.name, default_state, desc, opt.group_type, opt.group_name],
                )?;
                stats.options_indexed += 1;
            }

            for cap in re_opt_deps.captures_iter(&content) {
                if let (Some(k), Some(v)) = (cap.get(1), cap.get(2)) {
                    let opt_name = k.as_str().trim();
                    if defined_opts.iter().any(|o| o.name == opt_name) {
                        let dep_val = expand_vars(v.as_str(), &vars);
                        for dep_match in re_origin_extract.find_iter(&dep_val) {
                            let dep_origin = dep_match.as_str().to_string();
                            if dep_origin != origin {
                                tx.execute(
                                    "INSERT OR REPLACE INTO option_deps (port_origin, option_name, dep_origin, dep_type)
                                     VALUES (?1, ?2, ?3, 'RUN')",
                                    params![origin, opt_name, dep_origin],
                                )?;
                                stats.option_deps_indexed += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    tx.commit()?;
    Ok(stats)
}
