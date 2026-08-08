use crate::graph::DependencyGraph;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub struct ExporterStats {
    pub files_written: usize,
    pub options_saved: usize,
}

/// The markers around the region of make.conf this program owns. Everything
/// between them is rewritten on every export; everything outside them is the
/// user's — CFLAGS, MAKE_JOBS_NUMBER, whatever else lives in a real
/// make.conf — and is preserved byte for byte. Matched by exact trimmed line,
/// and invisible to the reader, which skips comments.
const BLOCK_BEGIN: &str = "# BEGIN bgone";
const BLOCK_END: &str = "# END bgone";

/// Replaces the bgone-managed block in `existing` with `block`, appending one
/// if none is there. `block` runs sentinel to sentinel inclusive.
///
/// Every block found is removed and the replacement spliced where the first
/// stood — a duplicated block would leave assignments the reader resolves by
/// file order. A `BEGIN` with no `END` swallows to end of file: everything
/// after an unterminated marker is this program's own truncated output, and
/// keeping it would duplicate the assignments being written. Output always
/// ends in a newline; a final line without one gains it, which is the one
/// byte-level liberty taken with the user's content.
fn splice_managed_block(existing: &str, block: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut insert_at: Option<usize> = None;
    let mut in_block = false;

    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == BLOCK_BEGIN {
            if insert_at.is_none() {
                insert_at = Some(kept.len());
            }
            in_block = true;
            continue;
        }
        if in_block {
            if trimmed == BLOCK_END {
                in_block = false;
            }
            continue;
        }
        kept.push(line);
    }

    let mut out: Vec<&str> = Vec::new();
    match insert_at {
        Some(at) => {
            out.extend(&kept[..at]);
            out.extend(block.lines());
            out.extend(&kept[at..]);
        }
        None => {
            out.extend(&kept);
            // A blank line between the user's content and the appended block,
            // so the file stays readable; replaced in place on later runs.
            if !kept.is_empty() && !kept.last().unwrap_or(&"").trim().is_empty() {
                out.push("");
            }
            out.extend(block.lines());
        }
    }

    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

/// Same-directory temp file then rename, so a crash mid-write can never leave
/// a half-written file where make — or the next session's reader — will find
/// it.
fn write_atomically(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("bgone.tmp");
    fs::write(&tmp, content).with_context(|| format!("Failed to write {:?}", tmp))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("Failed to move {:?} into place at {:?}", tmp, path))?;
    Ok(())
}

pub fn export_options(
    graph: &DependencyGraph,
    options_dir: &Path,
    dry_run: bool,
    make_conf_path: Option<&PathBuf>,
) -> Result<ExporterStats> {
    let mut stats = ExporterStats {
        files_written: 0,
        options_saved: 0,
    };

    // Group options by port origin (e.g., "www/apache24" -> "www_apache24").
    //
    // Only ports something currently pulls in are written. A port stranded by
    // an option being turned off is not part of the build, so writing an options
    // file for it would leave stale configuration behind for a port that is not
    // there — its selections stay in memory instead, ready if it comes back.
    let mut port_options: HashMap<String, Vec<(String, bool)>> = HashMap::new();

    for opt in graph
        .real_options()
        .filter(|opt| graph.is_live(&opt.port_origin))
    {
        port_options
            .entry(opt.port_origin.clone())
            .or_default()
            .push((opt.name.clone(), opt.enabled));
    }

    // Sorted so repeated runs write files in a stable order
    let mut origins: Vec<String> = port_options.keys().cloned().collect();
    origins.sort();

    for origin in origins {
        let parts: Vec<&str> = origin.split('/').collect();
        if parts.len() != 2 {
            continue;
        }

        let dir_name = format!("{}_{}", parts[0], parts[1]);
        let target_dir = options_dir.join(&dir_name);
        let target_file = target_dir.join("options");

        // Sorted so repeated runs produce byte-identical files
        let mut opts = port_options[&origin].clone();
        opts.sort_by(|a, b| a.0.cmp(&b.0));

        let pkg_name = graph
            .pkg_names
            .get(&origin)
            .cloned()
            .unwrap_or_else(|| parts[1].to_string());
        let complete_list: Vec<&str> = opts.iter().map(|(name, _)| name.as_str()).collect();

        // Mirrors the file `make config` writes (bsd.port.mk, do-config). Only
        // OPTIONS_FILE_SET / OPTIONS_FILE_UNSET are actually consumed, by
        // bsd.options.mk; the two underscore-prefixed headers are informational,
        // but `make showconfig` and human readers expect them.
        //
        // Every option the port defines must appear in one list or the other:
        // bsd.port.mk's `config-conditional` re-opens the dialog whenever
        // NEW_OPTIONS (COMPLETE_OPTIONS_LIST minus everything named here) is
        // non-empty, so an option omitted because the user never touched it is
        // enough to make poudriere prompt for the whole port again.
        let mut content = String::new();
        content.push_str("# This file is auto-generated by bgone.\n");
        content.push_str(&format!("# Options for {}\n", pkg_name));
        content.push_str(&format!("_OPTIONS_READ={}\n", pkg_name));
        content.push_str(&format!(
            "_FILE_COMPLETE_OPTIONS_LIST={}\n",
            complete_list.join(" ")
        ));

        for (opt_name, enabled) in &opts {
            if *enabled {
                content.push_str(&format!("OPTIONS_FILE_SET+={}\n", opt_name));
            } else {
                content.push_str(&format!("OPTIONS_FILE_UNSET+={}\n", opt_name));
            }
            stats.options_saved += 1;
        }

        if dry_run {
            println!("\n[DRY RUN] Would write to {:?}:\n---", target_file);
            print!("{}", content);
            println!("---");
            stats.files_written += 1;
        } else {
            fs::create_dir_all(&target_dir).with_context(|| {
                format!("Failed to create options directory at {:?}", target_dir)
            })?;
            write_atomically(&target_file, &content)
                .with_context(|| format!("Failed to write options file at {:?}", target_file))?;
            stats.files_written += 1;
        }
    }

    // Optional: Export global make.conf snippet if configured. Only the
    // managed block is bgone's; the file itself may be the system's real
    // /etc/make.conf, and overwriting it wholesale would destroy everything
    // else the user keeps there.
    if let Some(m_path) = make_conf_path {
        let mut make_content = String::new();
        make_content.push_str(BLOCK_BEGIN);
        make_content.push('\n');
        make_content.push_str("# Global option overrides managed by bgone. Everything between\n");
        make_content.push_str("# these markers is rewritten on save; everything outside them\n");
        make_content.push_str("# is preserved.\n");

        // An option name means different things to different ports, so a global
        // is only honest where every port that has it agrees. Deduping on
        // `(name, enabled)` instead emitted both `OPTIONS_SET+=X` and
        // `OPTIONS_UNSET+=X` whenever they disagreed, leaving the result to
        // whichever `bsd.options.mk` applied last.
        //
        // Where they disagree the per-port files already say so exactly, so the
        // name is left out of the global rather than guessed at.
        let mut states: BTreeMap<&str, Option<bool>> = BTreeMap::new();
        for opt in graph
            .real_options()
            .filter(|opt| graph.is_live(&opt.port_origin))
        {
            states
                .entry(opt.name.as_str())
                .and_modify(|agreed| {
                    if *agreed != Some(opt.enabled) {
                        *agreed = None;
                    }
                })
                .or_insert(Some(opt.enabled));
        }

        for (name, agreed) in &states {
            let Some(enabled) = agreed else {
                continue;
            };
            let key = if *enabled {
                "OPTIONS_SET"
            } else {
                "OPTIONS_UNSET"
            };
            make_content.push_str(&format!("{}+={}\n", key, name));
        }
        make_content.push_str(BLOCK_END);
        make_content.push('\n');

        // Only a file that is genuinely absent starts from nothing: splicing
        // over an unreadable file would write it back without whatever could
        // not be read.
        let existing = match fs::read_to_string(m_path) {
            Ok(content) => content,
            Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("could not read existing make.conf at {:?}", m_path))
            }
        };
        let merged = splice_managed_block(&existing, &make_content);

        if dry_run {
            println!("\n[DRY RUN] Would write make.conf to {:?}:\n---", m_path);
            print!("{}", merged);
            println!("---");
        } else {
            if let Some(parent) = m_path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_atomically(m_path, &merged)?;
            println!("[+] Global options written to {:?}", m_path);
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "# BEGIN bgone\nOPTIONS_SET+=SSL\n# END bgone\n";

    /// Nothing to preserve: the block is the file.
    #[test]
    fn splicing_into_nothing_is_just_the_block() {
        assert_eq!(splice_managed_block("", BLOCK), BLOCK);
    }

    /// The user's content survives on both sides, byte for byte.
    #[test]
    fn splicing_preserves_everything_outside_the_markers() {
        let existing =
            "CFLAGS+=-O2\n\n# BEGIN bgone\nOPTIONS_SET+=OLD\n# END bgone\nMAKE_JOBS_NUMBER=4\n";
        let merged = splice_managed_block(existing, BLOCK);
        assert_eq!(
            merged,
            "CFLAGS+=-O2\n\n# BEGIN bgone\nOPTIONS_SET+=SSL\n# END bgone\nMAKE_JOBS_NUMBER=4\n"
        );
    }

    /// With no block present, the block is appended after a separating blank,
    /// and a final line missing its newline gains one.
    #[test]
    fn splicing_appends_when_no_block_exists() {
        let merged = splice_managed_block("CFLAGS+=-O2", BLOCK);
        assert_eq!(merged, format!("CFLAGS+=-O2\n\n{BLOCK}"));
    }

    /// Splicing twice changes nothing: the block replaces itself in place.
    #[test]
    fn splicing_is_idempotent() {
        let once = splice_managed_block("CFLAGS+=-O2\n", BLOCK);
        let twice = splice_managed_block(&once, BLOCK);
        assert_eq!(once, twice);
    }

    /// A BEGIN with no END swallows to end of file — everything after an
    /// unterminated marker is this program's own truncated output — and the
    /// next splice recovers cleanly.
    #[test]
    fn a_missing_end_marker_truncates_and_recovers() {
        let existing = "CFLAGS+=-O2\n# BEGIN bgone\nOPTIONS_SET+=HALF";
        let merged = splice_managed_block(existing, BLOCK);
        assert_eq!(merged, format!("CFLAGS+=-O2\n{BLOCK}"));
    }

    /// Two blocks collapse to one at the first position; assignments must not
    /// survive in a second block for the reader's last-wins order to pick up.
    #[test]
    fn duplicate_blocks_collapse_to_one() {
        let existing = "# BEGIN bgone\nOPTIONS_SET+=A\n# END bgone\nCFLAGS+=-O2\n# BEGIN bgone\nOPTIONS_SET+=B\n# END bgone\n";
        let merged = splice_managed_block(existing, BLOCK);
        assert_eq!(merged, format!("{BLOCK}CFLAGS+=-O2\n"));
    }
}
