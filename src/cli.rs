use crate::config::Config;
use anyhow::Result;
use clap::parser::ValueSource;
use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
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
   / or Ctrl + F      Narrow the list to ports whose origin contains what you
                      type: postgres, databases/, py-. Options are not searched,
                      and a port that matches is shown whole. Enter keeps the
                      filter, Esc clears it, and the header says how many ports
                      a kept filter is showing.

                      Up / Down / PgUp / PgDn move through the results while
                      the bar is still open, so there is no need to commit to a
                      query before looking at what it found.

 GROUPS
   Ports in a group keep their option choices in step: setting an option on
   one member sets it on every member that has one by that name. That is what
   stops a family like lang/php8*-extensions drifting apart.

   Ctrl + G           Add the port under the cursor to a group, or start a new
                      one. Works from an option row too, acting on the port
                      that row belongs to.
   Ctrl + G  twice    Manage groups and membership, and save them to the config
                      file. A terminal cannot tell Ctrl + Shift + G from
                      Ctrl + G, so scope escalates by repetition here as well.

   Groups are stored in the --config file. With one given, joining or leaving
   a group writes it out straight away; with none, changes last only for the
   session until you save from the manager, which asks where to put one.

   Backspace also answers to Ctrl + H, since terminals disagree about which
   byte the key sends.

 TYPING IN A FIELD
   The search bar and the group prompts all edit the same way.

   Left / Right       Move the caret; typing and rubbing out happen there
                      rather than always at the end
   Home / End         Jump to either edge
   Up / Down          In the search bar only, move the list cursor rather than
                      the caret. The other prompts use them for their own lists.
   Backspace          Take the character behind the caret
   Delete             Take the one in front of it

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

    /// Read settings from a TOML config file.
    ///
    /// Anything the file sets overrides the built-in default; anything given
    /// explicitly on the command line overrides the file. Settings the file
    /// leaves out are simply unspecified. There is no default config file, and
    /// none is looked for unless this is given.
    #[arg(long, value_name = "FILE", global = true)]
    pub config: Option<PathBuf>,

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
    /// Evaluate a local FreeBSD ports tree into SQLite.
    ///
    /// Every port is evaluated by make, which is what makes the result exact:
    /// `.if`, `.for`, MASTERDIR, `.include` and Mk/Uses all resolve because the
    /// ports framework resolves them. It is the slow part of using bgone, and
    /// the only part that needs a ports tree — configuring afterwards reads
    /// nothing but the cache.
    Index {
        /// Path to the ports tree root
        #[arg(short, long, default_value = "/usr/ports")]
        ports_dir: PathBuf,

        /// Discard previous database cache before indexing
        #[arg(short, long)]
        force: bool,

        /// Resolve as this architecture rather than the host's.
        ///
        /// Which options a port defines can depend on it — `OPTIONS_DEFINE_${ARCH}`,
        /// `OPTIONS_EXCLUDE_${OPSYS}` — so a cache built on amd64 does not
        /// necessarily describe an aarch64 jail.
        #[arg(long, value_name = "ARCH")]
        jail_arch: Option<String>,

        /// Resolve as this OSVERSION (e.g. 1404000 for FreeBSD 14.4)
        #[arg(long, value_name = "N")]
        osversion: Option<String>,

        /// Resolve as this OPSYS (default: the host's)
        #[arg(long, value_name = "NAME")]
        opsys: Option<String>,

        /// Resolve as this OSREL (e.g. 14.4)
        #[arg(long, value_name = "VERSION")]
        osrel: Option<String>,
    },
}

/// Parses the command line, then lets a `--config` file fill in anything that
/// was not given explicitly.
///
/// Parsed through `ArgMatches` rather than `Cli::parse()` because the whole
/// precedence rule turns on telling a value that was *passed* from one that was
/// *defaulted*, and only the matches carry that. Returns the resolved arguments
/// and the config itself, which the caller still needs for its groups.
pub fn parse_with_config() -> Result<(Cli, Option<Config>)> {
    let matches = Cli::command().get_matches();
    let mut cli = Cli::from_arg_matches(&matches)?;

    let config = match &cli.config {
        Some(path) => Some(Config::load(path)?),
        None => None,
    };

    if let Some(config) = &config {
        cli.apply_config(&matches, config);
    }

    Ok((cli, config))
}

/// Resolves an argument list against an already-loaded config.
///
/// The same resolution [`parse_with_config`] performs, minus reading the file
/// and minus the exit-on-`--help` behaviour of `get_matches`, so precedence can
/// be exercised directly.
pub fn resolve_from<I, T>(args: I, config: Option<&Config>) -> Result<Cli>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let matches = Cli::command().try_get_matches_from(args)?;
    let mut cli = Cli::from_arg_matches(&matches)?;
    if let Some(config) = config {
        cli.apply_config(&matches, config);
    }
    Ok(cli)
}

/// True when `id` was given on the command line rather than left to its default.
///
/// `ArgAction::SetTrue` gives a flag an implicit default, so an unpassed flag
/// reports `DefaultValue` rather than nothing — which is what lets flags and
/// valued arguments share this one test.
fn was_passed(matches: &ArgMatches, id: &str) -> bool {
    matches.value_source(id) == Some(ValueSource::CommandLine)
}

impl Cli {
    /// Applies `config` to every argument the command line did not set.
    fn apply_config(&mut self, matches: &ArgMatches, config: &Config) {
        // Each argument resolves on its own, so a config supplying `file` and a
        // command line supplying origins leaves both in play — `collect_targets`
        // merges them afterwards exactly as it always has.
        macro_rules! fill {
            ($field:ident, $id:literal) => {
                if !was_passed(matches, $id) {
                    if let Some(value) = config.$field.clone() {
                        self.$field = value;
                    }
                }
            };
            (opt $field:ident, $id:literal) => {
                if !was_passed(matches, $id) {
                    if let Some(value) = config.$field.clone() {
                        self.$field = Some(value);
                    }
                }
            };
        }

        fill!(db_path, "db_path");
        fill!(options_dir, "options_dir");
        fill!(opt make_conf, "make_conf");
        fill!(opt file, "file");
        fill!(origins, "origins");
        fill!(dry_run, "dry_run");
        fill!(force_reset, "force_reset");
        fill!(ignore_missing, "ignore_missing");

        // `ports_dir` belongs to the subcommand, so its source lives there too
        if let (Some(Commands::Index { ports_dir, .. }), Some(sub)) =
            (self.command.as_mut(), matches.subcommand_matches("index"))
        {
            if !was_passed(sub, "ports_dir") {
                if let Some(value) = config.ports_dir.clone() {
                    *ports_dir = value;
                }
            }
        }
    }

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
