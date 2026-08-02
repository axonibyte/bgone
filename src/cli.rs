use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "bgone")]
#[command(version)]
#[command(about = "Fast reactive TUI tree configuration tool for FreeBSD ports")]
#[command(after_help = "\
KEYBINDINGS:

 SHOWING AND HIDING ROWS
   Three widths. Expand with = + ++, collapse with the shifted - _ __:

   =  /  -            Just the row under the cursor. Whatever is nested inside
                      keeps the shape it already had, so re-opening the row
                      brings back the same view.
   +  /  _            That row and everything nested inside it, however deep.
   ++ /  __           The whole list. Press + or _ a second time, with no
                      other key in between, to widen what you just did.

 MOVING AROUND
   Up / Down          One row (hold Ctrl to move five)
   Shift + Up / Down  Previous / next sibling, stepping over anything nested
                      inside them. Past the last sibling it moves outward, to
                      the parent's next entry.
   PgUp / PgDn        One screen
   Home / End         First / last row

 FOLLOWING RELATIONSHIPS
   Every port is listed once, so a dependency is shown as a reference rather
   than by nesting the whole port underneath.

   Enter              On a 'depends on', 'requires' or 'required by' entry,
                      jump to that port's own entry and open it.
   Backspace          Go back the way you came.

 CHOOSING OPTIONS
   Space              Turn the highlighted option on or off, or pick it when
                      it belongs to a radio group
   / or Ctrl + F      Filter rows by name, description or group
                      (Enter keeps the filter, Esc clears it)

 BUTTONS AND EXITING
   Tab / Shift + Tab  Move the highlight between the list and the
                      < OK > and < Cancel > buttons along the bottom
   Left / Right       Move between the two buttons
   Enter              Press the highlighted button. In the list it presses
                      < OK >, unless the cursor is on a relationship entry,
                      which it follows instead.
   o  /  c            Press < OK > / < Cancel > from anywhere
   Ctrl + S  or  s    Save the options and exit (same as < OK >)
   q  or  Esc         Exit without saving (same as < Cancel >). Asks you to
                      confirm first if any option has changed.

 REDRAWING
   Ctrl + L           Scroll the cursor row to the middle of the screen; press
                      again for the top, again for the bottom. Also repaints.
   Ctrl + R           Repaint the screen, leaving the view where it is.
")]
// NOTE: `args_conflicts_with_subcommands` is deliberately *not* set. It made
// `bgone --db-path cache.db index ...` a parse error, so the cache used by
// `index` could never be pointed anywhere but the default. `db_path` is global
// instead, and so is accepted on either side of the subcommand.
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Path to the SQLite cache database
    #[arg(short = 'd', long, default_value = "bgone_cache.db", global = true)]
    pub db_path: PathBuf,

    /// Directory to output FreeBSD option files
    #[arg(short, long, default_value = "/var/db/ports")]
    pub options_dir: PathBuf,

    /// Optional path to export/read a global make.conf snippet
    #[arg(short, long)]
    pub make_conf: Option<PathBuf>,

    /// Perform a dry-run without writing files to disk
    #[arg(short = 'n', long)]
    pub dry_run: bool,

    /// Skip asking the ports tree about the targeted ports.
    ///
    /// That pass exists to catch the roughly 1% of ports whose options the
    /// Makefile sweep cannot see, which are the ones poudriere would keep
    /// prompting for. Skipping it avoids a one-time cost on a cold cache and
    /// costs nothing else beyond exact package names in the written headers.
    #[arg(long)]
    pub no_describe: bool,

    /// Discard previous database cache and rebuild schema
    #[arg(short = 'r', long)]
    pub force_reset: bool,

    /// Warn instead of bailing out on missing/unmatched ports (unless 0 total ports match)
    #[arg(short = 'i', long)]
    pub ignore_missing: bool,

    /// Read target port origins/patterns from a file (space, tab, newline, or comma-delimited)
    #[arg(short = 'f', long, value_name = "FILE")]
    pub file: Option<PathBuf>,

    /// Target port origin(s) or glob pattern(s) (e.g. www/apache24 "www/py-*")
    #[arg(value_name = "ORIGIN")]
    pub origins: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Index a local FreeBSD ports tree directory into SQLite
    Index {
        /// Path to the ports tree root
        #[arg(short, long, default_value = "/usr/ports")]
        ports_dir: PathBuf,

        /// Discard previous database cache before indexing
        #[arg(short, long)]
        force: bool,
    },
}

impl Cli {
    /// Collects every requested target, merging positional origins with the
    /// contents of `--file` (if one was supplied).
    pub fn collect_targets(&self) -> Result<Vec<String>> {
        let mut targets = self.origins.clone();
        if let Some(ref file_path) = self.file {
            targets.append(&mut parse_ports_file(file_path)?);
        }
        Ok(targets)
    }
}

/// Reads a list of port origins/patterns from a file. Entries may be separated
/// by commas or any whitespace, and `#` starts a comment that runs to EOL.
pub fn parse_ports_file(path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(path)?;
    Ok(parse_ports_list(&content))
}

/// Splits the raw contents of a port-list file into individual origins/patterns.
pub fn parse_ports_list(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|line| line.split('#').next().unwrap_or("")) // Strip comment portion
        .flat_map(|line| line.split(|c: char| c == ',' || c.is_whitespace()))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}
