use crate::config::Config;
use crate::poudriere::Poudriere;
use anyhow::{bail, Result};
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

 NARROWING THE LIST
   Shift + H          Hide the ports that define no options at all, and show
                      them again. Most of a build set is leaf libraries with
                      nothing to decide about them; this leaves only the ports
                      there is a choice to make about. A port make could not
                      read is never hidden - its options are unknown rather
                      than absent, which is the one thing worth seeing.

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

                      A port joining a group takes on the choices the members
                      already there have made, since being in the group is the
                      statement that it should be configured like them. Names
                      it does not define are skipped, and names only it has are
                      left as they were. The first port in a group has nobody
                      to copy and keeps what it had.

                      A port in a group is labelled with the group's name, in
                      cyan, on its row.
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
   s                  Save the options and exit (same as < OK >)
   Ctrl + S           Write the options out and carry on. A long session's
                      picking should not be riding on getting out cleanly at
                      the end of it.
   q / Esc / Ctrl + C Leave. Always asks first, whether or not anything has
                      changed, offering < Save and exit >, < Discard > and
                      < Cancel >; Esc or Ctrl + C a second time discards
                      without waiting to be pointed at a button.

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

    /// Path to the ports tree root.
    ///
    /// Every fact about a port comes from evaluating it here with make, so this
    /// is needed to configure and not only to preheat. `--poudriere-ports`
    /// derives it from a poudriere tree instead of naming it.
    #[arg(short = 'p', long, default_value = "/usr/ports", global = true)]
    pub ports_dir: PathBuf,

    /// Resolve as this architecture rather than the host's.
    ///
    /// Which options a port defines can depend on it — `OPTIONS_DEFINE_${ARCH}`,
    /// `OPTIONS_EXCLUDE_${OPSYS}` — so what is right for amd64 does not
    /// necessarily describe an aarch64 jail.
    #[arg(long, value_name = "ARCH", global = true)]
    pub jail_arch: Option<String>,

    /// Resolve as this OSVERSION (e.g. 1404000 for FreeBSD 14.4)
    #[arg(long, value_name = "N", global = true)]
    pub osversion: Option<String>,

    /// Resolve as this OPSYS (default: the host's)
    #[arg(long, value_name = "NAME", global = true)]
    pub opsys: Option<String>,

    /// Resolve as this OSREL (e.g. 14.4)
    #[arg(long, value_name = "VERSION", global = true)]
    pub osrel: Option<String>,

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

    /// poudriere's configuration root, holding `poudriere.d`.
    ///
    /// Only consulted when one of the other --poudriere-* arguments is given.
    #[arg(
        long,
        value_name = "DIR",
        default_value = "/usr/local/etc",
        global = true
    )]
    pub poudriere_etc: PathBuf,

    /// Take the architecture and OS version from this poudriere jail.
    ///
    /// Saves transcribing --jail-arch, --osversion and --osrel, and cannot
    /// disagree with the jail the way a hand-copied value can.
    #[arg(long, value_name = "JAIL", global = true)]
    pub poudriere_jail: Option<String>,

    /// Take the ports tree path from this poudriere ports tree, and name the
    /// options directory after it.
    #[arg(long, value_name = "TREE", global = true)]
    pub poudriere_ports: Option<String>,

    /// The poudriere set, if you use one. Only affects the options directory.
    #[arg(long, value_name = "SET", global = true)]
    pub poudriere_set: Option<String>,

    /// Name the options directory outright rather than composing it from the
    /// jail, tree and set — the same escape hatch as `poudriere options -o`.
    #[arg(long, value_name = "NAME", global = true)]
    pub poudriere_optionsdir: Option<String>,

    /// Target port origin(s) or glob pattern(s) (e.g. www/apache24 "www/py-*")
    #[arg(value_name = "ORIGIN")]
    pub origins: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Fill the cache ahead of time, so a later session starts warm.
    ///
    /// Optional. Nothing needs indexing before configuring: every fact is
    /// learned by evaluating a port with make, and the cache remembers what make
    /// said against the exact question asked — which port, resolved as what,
    /// from Makefiles of what age, under which options. A configure run fills it
    /// as it goes.
    ///
    /// This runs the same evaluations up front. With no targets it preheats the
    /// same ports the configure run would; `--all` preheats the whole tree,
    /// which takes a while and is rarely what you want.
    Index {
        /// Discard the cache before preheating
        #[arg(short, long)]
        force: bool,

        /// Preheat every port in the tree rather than the targets given.
        ///
        /// Roughly 35,000 evaluations. A build set is a few hundred, so this is
        /// worth it only if you configure across the whole tree.
        #[arg(short, long)]
        all: bool,

        /// Ports to preheat. Defaults to the same targets a configure run would
        /// take, from `-f` and from origins given before the subcommand.
        ///
        /// Positional arguments cannot be global in clap, so this exists to let
        /// `bgone index www/nginx` read the way it looks like it should.
        #[arg(value_name = "ORIGIN")]
        origins: Vec<String>,
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

    cli.resolve(&matches, config.as_ref())?;

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
    cli.resolve(&matches, config)?;
    Ok(cli)
}

/// True when `id` was given on the command line rather than left to its default.
///
/// `ArgAction::SetTrue` gives a flag an implicit default, so an unpassed flag
/// reports `DefaultValue` rather than nothing — which is what lets flags and
/// valued arguments share this one test.
fn was_passed(matches: &ArgMatches, id: &str) -> bool {
    // `value_source` panics on an id the matches has never heard of, and a
    // subcommand's matches only carries the *global* arguments — so asking it
    // about a top-level one has to be a no rather than an abort.
    if matches.try_get_raw(id).is_err() {
        return false;
    }
    matches.value_source(id) == Some(ValueSource::CommandLine)
}

/// What a poudriere spec resolves to, once the attribute store has been read.
///
/// Every field is optional because a spec need not name everything: a jail
/// alone yields the resolution target, a tree alone yields a ports directory
/// and an options directory.
#[derive(Debug, Default, Clone)]
struct Derived {
    jail_arch: Option<String>,
    osversion: Option<String>,
    osrel: Option<String>,
    ports_dir: Option<PathBuf>,
    options_dir: Option<PathBuf>,
}

/// Reads one tier's poudriere spec.
///
/// `None` when the tier names nothing, which is the common case — most runs
/// mention poudriere at neither tier and this never touches the filesystem.
fn derive(
    etc: &Path,
    jail: Option<&str>,
    tree: Option<&str>,
    set: Option<&str>,
    optionsdir: Option<&str>,
) -> Result<Option<Derived>> {
    if jail.is_none() && tree.is_none() && set.is_none() && optionsdir.is_none() {
        return Ok(None);
    }

    let poudriere = Poudriere::new(etc);
    if !poudriere.is_available() {
        bail!(
            "--poudriere-* was given but {} does not exist. \
             Drop the argument to configure without poudriere.",
            poudriere.root().display()
        );
    }

    let mut derived = Derived::default();

    if let Some(jail) = jail {
        // A named jail that is not there is an error, not a fallback: you asked
        // for something specific.
        let facts = poudriere.jail(jail)?;
        derived.jail_arch = Some(facts.arch);
        derived.osrel = Some(facts.osrel);
        derived.osversion = Some(facts.osversion);
    }

    if let Some(tree) = tree {
        derived.ports_dir = Some(poudriere.ports_dir(tree)?);
    }

    derived.options_dir = Some(match optionsdir {
        Some(name) => poudriere.named_options_dir(name),
        None => poudriere.options_dir(jail, tree, set),
    });

    Ok(Some(derived))
}

/// The first tier that says anything.
///
/// Precedence is expressed by ordering candidates rather than by overwriting in
/// sequence. Overwriting cannot express four tiers here: `was_passed` only
/// distinguishes "typed on the command line" from everything else, so once a
/// derived value has been written there is nothing left to stop the config tier
/// replacing it.
fn first_set<T>(candidates: [Option<T>; 4]) -> Option<T> {
    candidates.into_iter().flatten().next()
}

impl Cli {
    /// Resolves every setting against all five tiers, in order:
    ///
    /// 1. arguments typed on the command line
    /// 2. values derived from a `--poudriere-*` spec given on the command line
    /// 3. settings written explicitly in the config file
    /// 4. values derived from a poudriere spec in the config file
    /// 5. clap's own defaults, already in place
    ///
    /// Outer key is command-line-beats-config; inner key is
    /// explicit-beats-derived. So a `--poudriere-jail` typed at the prompt does
    /// override a `jail_arch` written in the config — naming a jail on the
    /// command line is a deliberate act, and command-line-beats-config is the
    /// rule that stays predictable when the two interleave.
    fn resolve(&mut self, matches: &ArgMatches, config: Option<&Config>) -> Result<()> {
        let cli_spec = derive(
            &self.poudriere_etc,
            self.poudriere_jail.as_deref(),
            self.poudriere_ports.as_deref(),
            self.poudriere_set.as_deref(),
            self.poudriere_optionsdir.as_deref(),
        )?;

        let cfg_spec = match config {
            Some(config) => {
                let etc = config
                    .poudriere_etc
                    .clone()
                    .unwrap_or_else(|| self.poudriere_etc.clone());
                derive(
                    &etc,
                    config.poudriere_jail.as_deref(),
                    config.poudriere_ports.as_deref(),
                    config.poudriere_set.as_deref(),
                    config.poudriere_optionsdir.as_deref(),
                )?
            }
            None => None,
        };

        // Tier 1 is "was it typed", which for a top-level argument is answered
        // by the top-level matches and for a subcommand argument by the
        // subcommand's own — the top-level matches has no id for those at all.
        let sub = matches.subcommand_matches("index");

        // Every one of these is global, so a value typed on the command line is
        // in the top-level matches wherever it appeared — `bgone index -p …` and
        // `bgone -p … index` resolve alike. `sub` is still consulted because
        // clap records a global argument against the subcommand it was typed
        // after as well.
        macro_rules! tiers {
            ($id:literal, $derived:ident, $cfg:expr, $current:expr) => {{
                let typed =
                    was_passed(matches, $id) || sub.map(|m| was_passed(m, $id)).unwrap_or(false);
                first_set([
                    if typed { Some($current) } else { None },
                    cli_spec.as_ref().and_then(|d| d.$derived.clone()),
                    $cfg,
                    cfg_spec.as_ref().and_then(|d| d.$derived.clone()),
                ])
            }};
        }

        if let Some(value) = tiers!(
            "options_dir",
            options_dir,
            config.and_then(|c| c.options_dir.clone()),
            self.options_dir.clone()
        ) {
            self.options_dir = value;
        }

        if let Some(value) = tiers!(
            "ports_dir",
            ports_dir,
            config.and_then(|c| c.ports_dir.clone()),
            self.ports_dir.clone()
        ) {
            self.ports_dir = value;
        }

        macro_rules! opt_tiers {
            ($field:ident, $id:literal, $derived:ident) => {
                let typed =
                    was_passed(matches, $id) || sub.map(|m| was_passed(m, $id)).unwrap_or(false);
                if let Some(value) = first_set([
                    if typed { self.$field.clone() } else { None },
                    cli_spec.as_ref().and_then(|d| d.$derived.clone()),
                    config.and_then(|c| c.$field.clone()),
                    cfg_spec.as_ref().and_then(|d| d.$derived.clone()),
                ]) {
                    self.$field = Some(value);
                }
            };
        }

        opt_tiers!(jail_arch, "jail_arch", jail_arch);
        opt_tiers!(osversion, "osversion", osversion);
        opt_tiers!(osrel, "osrel", osrel);

        // OPSYS is never derived: every poudriere jail is FreeBSD, so there is
        // nothing a spec could tell us that the default does not.
        let opsys_typed =
            was_passed(matches, "opsys") || sub.map(|m| was_passed(m, "opsys")).unwrap_or(false);
        if !opsys_typed {
            if let Some(value) = config.and_then(|c| c.opsys.clone()) {
                self.opsys = Some(value);
            }
        }

        if let Some(config) = config {
            self.apply_config(matches, config);
        }
        Ok(())
    }

    /// Applies `config` to every argument the command line did not set.
    ///
    /// Only the settings poudriere never derives reach here; the rest are
    /// resolved by [`Cli::resolve`] before this runs, and are not touched again.
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
        fill!(opt make_conf, "make_conf");
        fill!(opt file, "file");
        fill!(origins, "origins");
        fill!(dry_run, "dry_run");
        fill!(force_reset, "force_reset");
        fill!(ignore_missing, "ignore_missing");
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
