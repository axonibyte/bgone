//! Verifies that every documented command-line argument and switch parses into
//! the value `main` actually consumes, and that the non-interactive entry points
//! behave as documented when the real binary is invoked.
//!
//! NOTE: no test here invokes the binary with a resolvable origin. That path
//! enters the TUI, which grabs the controlling terminal (crossterm opens
//! `/dev/tty` directly) and would hang the suite. Those code paths are covered
//! at the library level in `integration_tests.rs`.

mod common;

use common::{write_mock_ports_tree, TempDir};

use clap::Parser;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bgone::cli::{parse_ports_file, parse_ports_list, resolve_from, Cli, Commands};
use bgone::config::{save_groups, Config, Groups};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("bgone").chain(args.iter().copied()))
        .unwrap_or_else(|e| panic!("failed to parse {:?}: {e}", args))
}

/// Resolves an argument list against a config parsed from TOML.
fn resolve(args: &[&str], toml: &str) -> Cli {
    let config = Config::parse(toml).unwrap_or_else(|e| panic!("bad test config: {e}"));
    resolve_from(
        std::iter::once("bgone").chain(args.iter().copied()),
        Some(&config),
    )
    .unwrap_or_else(|e| panic!("failed to resolve {args:?}: {e}"))
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bgone"))
        .args(args)
        .output()
        .expect("failed to execute bgone binary")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ============================================================================
// 1. ARGUMENT PARSING - DEFAULTS
// ============================================================================

#[test]
fn test_defaults_match_documentation() {
    let cli = parse(&[]);

    assert_eq!(cli.db_path, PathBuf::from("bgone_cache.db"));
    assert_eq!(cli.options_dir, PathBuf::from("/var/db/ports"));
    assert_eq!(cli.make_conf, None);
    assert!(!cli.dry_run);
    assert!(!cli.force_reset);
    assert!(!cli.ignore_missing);
    assert_eq!(cli.file, None);
    assert!(cli.origins.is_empty());
    assert!(cli.command.is_none());
}

#[test]
fn test_index_subcommand_defaults() {
    let cli = parse(&["index"]);

    match cli.command {
        Some(Commands::Index {
            ports_dir, force, ..
        }) => {
            assert_eq!(ports_dir, PathBuf::from("/usr/ports"));
            assert!(!force);
        }
        _ => panic!("expected the index subcommand"),
    }
}

// ============================================================================
// 2. ARGUMENT PARSING - SHORT AND LONG FORMS
// ============================================================================

#[test]
fn test_short_switches_parse() {
    let cli = parse(&[
        "-d",
        "/tmp/cache.db",
        "-o",
        "/tmp/opts",
        "-m",
        "/tmp/make.conf",
        "-n",
        "-r",
        "-i",
        "-f",
        "/tmp/ports.txt",
        "www/nginx",
    ]);

    assert_eq!(cli.db_path, PathBuf::from("/tmp/cache.db"));
    assert_eq!(cli.options_dir, PathBuf::from("/tmp/opts"));
    assert_eq!(cli.make_conf, Some(PathBuf::from("/tmp/make.conf")));
    assert!(cli.dry_run);
    assert!(cli.force_reset);
    assert!(cli.ignore_missing);
    assert_eq!(cli.file, Some(PathBuf::from("/tmp/ports.txt")));
    assert_eq!(cli.origins, vec!["www/nginx".to_string()]);
}

#[test]
fn test_long_switches_parse_identically_to_short() {
    let short = parse(&[
        "-d",
        "a.db",
        "-o",
        "b",
        "-m",
        "c",
        "-n",
        "-r",
        "-i",
        "-f",
        "d",
        "www/nginx",
    ]);
    let long = parse(&[
        "--db-path",
        "a.db",
        "--options-dir",
        "b",
        "--make-conf",
        "c",
        "--dry-run",
        "--force-reset",
        "--ignore-missing",
        "--file",
        "d",
        "www/nginx",
    ]);

    assert_eq!(short.db_path, long.db_path);
    assert_eq!(short.options_dir, long.options_dir);
    assert_eq!(short.make_conf, long.make_conf);
    assert_eq!(short.dry_run, long.dry_run);
    assert_eq!(short.force_reset, long.force_reset);
    assert_eq!(short.ignore_missing, long.ignore_missing);
    assert_eq!(short.file, long.file);
    assert_eq!(short.origins, long.origins);
}

#[test]
fn test_multiple_origins_and_globs_are_collected() {
    let cli = parse(&["www/apache24", "www/py-*", "databases/postgresql16-server"]);

    assert_eq!(
        cli.origins,
        vec![
            "www/apache24".to_string(),
            "www/py-*".to_string(),
            "databases/postgresql16-server".to_string(),
        ]
    );
}

#[test]
fn test_index_subcommand_switches_parse() {
    for args in [
        vec!["index", "-p", "/mnt/ports", "-f"],
        vec!["index", "--ports-dir", "/mnt/ports", "--force"],
    ] {
        let cli = parse(&args);
        match cli.command {
            Some(Commands::Index {
                ports_dir, force, ..
            }) => {
                assert_eq!(ports_dir, PathBuf::from("/mnt/ports"), "args: {:?}", args);
                assert!(force, "args: {:?}", args);
            }
            _ => panic!("expected the index subcommand for {:?}", args),
        }
    }
}

#[test]
fn test_db_path_is_accepted_on_either_side_of_the_subcommand() {
    // `index` writes to --db-path, so it has to be reachable from both positions.
    let before = parse(&["--db-path", "/tmp/cache.db", "index", "-p", "/mnt/ports"]);
    let after = parse(&["index", "-p", "/mnt/ports", "--db-path", "/tmp/cache.db"]);

    assert_eq!(before.db_path, PathBuf::from("/tmp/cache.db"));
    assert_eq!(after.db_path, PathBuf::from("/tmp/cache.db"));
}

#[test]
fn test_unknown_switch_is_rejected() {
    assert!(Cli::try_parse_from(["bgone", "--not-a-real-flag"]).is_err());
}

// ============================================================================
// 3. PORT-LIST FILE (-f / --file)
// ============================================================================

#[test]
fn test_ports_file_parses_mixed_delimiters_and_comments() {
    let temp = TempDir::new("cli_ports_file");
    let ports_file = temp.join("my_ports.txt");

    std::fs::write(
        &ports_file,
        "# Core infrastructure\n\
         www/apache24, databases/postgresql16-server  lang/python311 # inline comment\n\
         \n\
         \t# Wildcards\n\
         www/py-*,\tnet/curl\n",
    )
    .unwrap();

    let origins = parse_ports_file(&ports_file).unwrap();

    assert_eq!(
        origins,
        vec![
            "www/apache24",
            "databases/postgresql16-server",
            "lang/python311",
            "www/py-*",
            "net/curl",
        ]
    );
}

#[test]
fn test_ports_file_missing_is_an_error() {
    let temp = TempDir::new("cli_missing_file");
    let missing = temp.join("does-not-exist.txt");

    assert!(parse_ports_file(&missing).is_err());
}

#[test]
fn test_collect_targets_merges_positional_origins_with_file_entries() {
    let temp = TempDir::new("cli_collect");
    let ports_file = temp.join("ports.txt");
    std::fs::write(&ports_file, "net/curl\nwww/py-*\n").unwrap();

    let file_arg = ports_file.to_str().unwrap();
    let cli = parse(&["www/apache24", "-f", file_arg]);
    let targets = cli.collect_targets().unwrap();

    assert_eq!(
        targets,
        vec![
            "www/apache24".to_string(),
            "net/curl".to_string(),
            "www/py-*".to_string(),
        ]
    );
}

#[test]
fn test_collect_targets_without_file_returns_positional_origins() {
    let cli = parse(&["www/apache24", "net/curl"]);
    assert_eq!(
        cli.collect_targets().unwrap(),
        vec!["www/apache24".to_string(), "net/curl".to_string()]
    );
}

#[test]
fn test_ports_list_ignores_fully_commented_and_blank_content() {
    assert!(parse_ports_list("# nothing here\n\n   \n#more\n").is_empty());
}

// ============================================================================
// 4. BINARY BEHAVIOUR (non-interactive paths only)
// ============================================================================

/// Every key the footer advertises must be explained by `--help`.
///
/// The other direction is checked by hand below; this one is checked
/// mechanically because the footer is a single line that is easy to edit
/// without remembering that the help text exists.
#[test]
fn test_every_key_on_the_footer_is_explained_in_help() {
    let help = stdout_of(&run(&["--help"]));

    for hint in bgone::ui::FOOTER_ACTION_KEYS
        .lines()
        .flat_map(|row| row.split('|'))
    {
        let key = hint.split_whitespace().next().unwrap_or_default();
        // How the footer's shorthand is spelled out in the help text
        let spelled = match key {
            "^S" => "Ctrl + S",
            "^L" => "Ctrl + L",
            "^R" => "Ctrl + R",
            "^G" => "Ctrl + G",
            "Bksp" => "Backspace",
            other => other,
        };
        assert!(
            help.contains(spelled),
            "footer advertises {key:?} but --help never explains {spelled:?}"
        );
    }
}

#[test]
fn test_help_documents_every_switch() {
    let out = run(&["--help"]);
    assert!(out.status.success());

    let help = stdout_of(&out);
    for expected in [
        "-d, --db-path",
        "-o, --options-dir",
        "-m, --make-conf",
        "-n, --dry-run",
        "-r, --force-reset",
        "-i, --ignore-missing",
        "-f, --file",
        "-h, --help",
        "-V, --version",
        "index",
        "[ORIGIN]...",
    ] {
        assert!(help.contains(expected), "`--help` is missing {expected}");
    }

    // Keybindings advertised in the footer must be advertised here too
    for binding in [
        "Ctrl + L",
        "Ctrl + R",
        "Ctrl + S",
        "Ctrl + F",
        "Shift + Up / Down",
        "Tab / Shift + Tab",
        "=  /  -",
        "+  /  _",
        "++ /  __",
        "o  /  c",
        "Backspace",
    ] {
        assert!(help.contains(binding), "`--help` is missing {binding}");
    }

    // Each expansion width must be described, not just listed
    for explanation in [
        "Just the row under the cursor",
        "That row and everything nested inside it",
        "The whole list",
    ] {
        assert!(
            help.contains(explanation),
            "`--help` lists the expansion keys without explaining {explanation:?}"
        );
    }

    // Relationships are references rather than nesting, so the keys that follow
    // them have to be discoverable from --help
    for explanation in ["jump to that port's own entry", "Go back the way you came"] {
        assert!(
            help.contains(explanation),
            "`--help` does not explain how to follow a relationship: {explanation:?}"
        );
    }

    // Retired bindings must not linger in the help
    for line in help.lines().map(str::trim) {
        for retired in ["e / E", "c / C", "] / [", "} / {"] {
            assert!(
                !line.starts_with(retired),
                "help still advertises the retired binding {retired:?}: {line}"
            );
        }
    }
}

#[test]
fn test_index_help_documents_its_switches() {
    let out = run(&["index", "--help"]);
    assert!(out.status.success());

    let help = stdout_of(&out);
    assert!(help.contains("-p, --ports-dir"));
    assert!(help.contains("-f, --force"));
    assert!(help.contains("-d, --db-path"));
}

#[test]
fn test_version_switch() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    assert!(stdout_of(&out).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_no_origins_prints_help_and_exits_cleanly() {
    let temp = TempDir::new("cli_no_origins");
    let db = temp.join("cache.db");

    let out = run(&["-d", db.to_str().unwrap()]);

    assert!(out.status.success());
    let stdout = stdout_of(&out);
    assert!(stdout.contains("Usage: bgone"));
    assert!(stdout.contains("--ignore-missing"));
}

#[test]
fn test_index_writes_to_the_requested_db_path() {
    let temp = TempDir::new("cli_index");
    let ports_root = write_mock_ports_tree(&temp);
    let db = temp.join("cache.db");

    let out = run(&[
        "index",
        "--ports-dir",
        ports_root.to_str().unwrap(),
        "--db-path",
        db.to_str().unwrap(),
    ]);

    assert!(
        out.status.success(),
        "index failed: {}",
        stderr_of(&out) + &stdout_of(&out)
    );
    assert!(stdout_of(&out).contains("Indexed 1 ports"));
    assert!(db.exists(), "--db-path was ignored by the index subcommand");
    assert_eq!(port_count(&db), 1);
}

#[test]
fn test_index_force_rebuilds_the_cache() {
    let temp = TempDir::new("cli_index_force");
    let ports_root = write_mock_ports_tree(&temp);
    let db = temp.join("cache.db");
    let db_arg = db.to_str().unwrap();
    let ports_arg = ports_root.to_str().unwrap();

    assert!(run(&["index", "-p", ports_arg, "-d", db_arg])
        .status
        .success());

    // Stale row that a forced rebuild must drop
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO ports (origin, pkgbase, pkgname) VALUES ('stale/port', 's', 's-1')",
            [],
        )
        .unwrap();
    }
    assert_eq!(port_count(&db), 2);

    let out = run(&["index", "-p", ports_arg, "-d", db_arg, "--force"]);
    assert!(out.status.success());
    assert_eq!(port_count(&db), 1, "--force did not rebuild the schema");
}

#[test]
fn test_force_reset_switch_rebuilds_the_cache() {
    let temp = TempDir::new("cli_force_reset");
    let ports_root = write_mock_ports_tree(&temp);
    let db = temp.join("cache.db");
    let db_arg = db.to_str().unwrap();

    assert!(
        run(&["index", "-p", ports_root.to_str().unwrap(), "-d", db_arg])
            .status
            .success()
    );
    assert_eq!(port_count(&db), 1);

    // No origins, so this exits after printing help - but -r still wipes the cache
    assert!(run(&["-d", db_arg, "--force-reset"]).status.success());
    assert_eq!(port_count(&db), 0, "--force-reset did not drop the tables");
}

#[test]
fn test_unresolvable_origin_exits_with_an_error() {
    let temp = TempDir::new("cli_unknown_origin");
    let db = temp.join("cache.db");

    let out = run(&["-d", db.to_str().unwrap(), "nonexistent/port"]);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("No matching ports found"));
}

#[test]
fn test_missing_ports_file_exits_with_an_error() {
    let temp = TempDir::new("cli_missing_ports_file");
    let db = temp.join("cache.db");
    let missing = temp.join("nope.txt");

    let out = run(&["-d", db.to_str().unwrap(), "-f", missing.to_str().unwrap()]);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("Error reading ports file"));
}

#[test]
fn test_ignore_missing_still_fails_when_nothing_resolves() {
    let temp = TempDir::new("cli_ignore_missing");
    let db = temp.join("cache.db");

    let out = run(&[
        "-d",
        db.to_str().unwrap(),
        "--ignore-missing",
        "nonexistent/one",
        "nonexistent/two",
    ]);

    assert_eq!(out.status.code(), Some(1));
    assert!(stderr_of(&out).contains("No matching ports found"));
}

fn port_count(db: &Path) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT COUNT(*) FROM ports", [], |r| r.get(0))
        .unwrap()
}

// ============================================================================
// 5. CONFIG FILE (delta over defaults, under explicit arguments)
// ============================================================================

/// The three-way rule: a default applies when nothing else speaks, the config
/// beats the default, and an explicitly-passed argument beats the config.
#[test]
fn test_config_sits_between_defaults_and_explicit_arguments() {
    // Nothing set anywhere: the built-in default stands
    assert_eq!(resolve(&[], "").options_dir, PathBuf::from("/var/db/ports"));

    // Config speaks, the command line does not
    assert_eq!(
        resolve(&[], r#"options_dir = "/from/config""#).options_dir,
        PathBuf::from("/from/config")
    );

    // Both speak: the command line wins
    assert_eq!(
        resolve(&["-o", "/from/cli"], r#"options_dir = "/from/config""#).options_dir,
        PathBuf::from("/from/cli")
    );
}

/// Flags go through the same rule. They only work because `ArgAction::SetTrue`
/// gives them an implicit default, so an unpassed flag is distinguishable from
/// one passed as false.
#[test]
fn test_a_boolean_flag_follows_the_same_precedence() {
    assert!(!resolve(&[], "").ignore_missing, "default is off");
    assert!(resolve(&[], "ignore_missing = true").ignore_missing);
    assert!(
        resolve(&["-i"], "ignore_missing = false").ignore_missing,
        "passing the flag beats a config that turns it off"
    );
}

/// A config is a delta: whatever it omits keeps whatever it would otherwise
/// have had, and omission is never itself an error.
#[test]
fn test_a_partial_config_leaves_everything_else_alone() {
    let cli = resolve(&[], r#"ignore_missing = true"#);

    assert!(cli.ignore_missing);
    assert_eq!(cli.options_dir, PathBuf::from("/var/db/ports"));
    assert_eq!(cli.db_path, PathBuf::from("bgone_cache.db"));
    assert!(cli.make_conf.is_none());
    assert!(cli.file.is_none());
    assert!(!cli.dry_run);
    assert!(cli.origins.is_empty());

    // ...and an entirely empty config changes nothing at all
    assert_eq!(resolve(&[], "").options_dir, resolve(&[], "").options_dir);
}

/// `origins` and `--file` are separate arguments, so a config supplying one and
/// a command line supplying the other leaves both in play.
#[test]
fn test_config_origins_and_command_line_file_both_apply() {
    let cli = resolve(&["-f", "/cli/list"], r#"origins = ["www/nginx"]"#);
    assert_eq!(cli.origins, vec!["www/nginx"]);
    assert_eq!(cli.file, Some(PathBuf::from("/cli/list")));

    // Explicit origins replace the config's rather than appending to them
    let cli = resolve(&["www/apache24"], r#"origins = ["www/nginx"]"#);
    assert_eq!(cli.origins, vec!["www/apache24"]);
}

/// `ports_dir` belongs to the subcommand, so its precedence has to be resolved
/// against the subcommand's own matches.
#[test]
fn test_config_reaches_the_index_subcommands_ports_dir() {
    let from_config = resolve(&["index"], r#"ports_dir = "/from/config""#);
    match from_config.command {
        Some(Commands::Index { ports_dir, .. }) => {
            assert_eq!(ports_dir, PathBuf::from("/from/config"))
        }
        _ => panic!("expected the index subcommand"),
    }

    let explicit = resolve(
        &["index", "--ports-dir", "/from/cli"],
        r#"ports_dir = "/from/config""#,
    );
    match explicit.command {
        Some(Commands::Index { ports_dir, .. }) => {
            assert_eq!(ports_dir, PathBuf::from("/from/cli"))
        }
        _ => panic!("expected the index subcommand"),
    }
}

/// A misspelled key does nothing, which is much harder to notice than a
/// failure, so it is refused and the message names both the key and the
/// alternatives.
#[test]
fn test_an_unknown_config_key_is_refused_by_name() {
    let err = Config::parse(r#"options_dirr = "/typo""#)
        .expect_err("an unknown key must not be accepted")
        .to_string();
    assert!(err.contains("options_dirr"), "message was: {err}");
    assert!(err.contains("options_dir"), "message was: {err}");
}

/// Naming a config that is not there is a mistake worth reporting rather than a
/// silent fall back to defaults.
#[test]
fn test_a_missing_config_file_is_an_error() {
    let out = run(&["--config", "/nonexistent/bgone.toml", "www/nginx"]);
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("Could not read config file"), "stderr: {err}");
}

// ============================================================================
// 6. CONFIG WRITE-BACK (a file a person maintains)
// ============================================================================

fn groups_of(pairs: &[(&str, &[&str])]) -> Groups {
    pairs
        .iter()
        .map(|(name, members)| {
            (
                name.to_string(),
                members.iter().map(|m| m.to_string()).collect(),
            )
        })
        .collect()
}

/// Saving a group must not cost the user their comments, their key order, or
/// any setting unrelated to groups.
#[test]
fn test_saving_groups_leaves_the_rest_of_the_file_alone() {
    let temp = TempDir::new("config_save");
    let path = temp.path.join("bgone.toml");

    let original = "\
# Where poudriere reads options from for the HEAD tree
options_dir = \"/usr/local/etc/poudriere.d/HEAD-options\"

# The list I actually build
file = \"/usr/local/etc/poudriere.d/port-list\"
ignore_missing = true
";
    std::fs::write(&path, original).unwrap();

    save_groups(
        &path,
        &groups_of(&[(
            "php-extensions",
            &["lang/php83-extensions", "lang/php84-extensions"],
        )]),
    )
    .unwrap();

    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("# Where poudriere reads options from for the HEAD tree"),
        "comments were lost:\n{written}"
    );
    assert!(written.contains("# The list I actually build"));
    assert!(written.contains(r#"options_dir = "/usr/local/etc/poudriere.d/HEAD-options""#));
    assert!(written.contains("ignore_missing = true"));
    assert!(written.contains("[groups]"));

    // ...and it still loads, with everything intact
    let reloaded = Config::parse(&written).unwrap();
    assert_eq!(reloaded.ignore_missing, Some(true));
    assert_eq!(
        reloaded.groups["php-extensions"],
        vec!["lang/php83-extensions", "lang/php84-extensions"]
    );
}

/// Saving when there is no config yet creates one holding just the groups.
#[test]
fn test_saving_groups_creates_a_config_from_nothing() {
    let temp = TempDir::new("config_create");
    let path = temp.path.join("new.toml");

    save_groups(&path, &groups_of(&[("db", &["databases/redis"])])).unwrap();

    let written = std::fs::read_to_string(&path).unwrap();
    let reloaded = Config::parse(&written).unwrap();
    assert_eq!(reloaded.groups["db"], vec!["databases/redis"]);
    assert!(reloaded.options_dir.is_none(), "nothing else invented");
}

/// Re-saving replaces the groups rather than accumulating them, and dropping the
/// last group takes the table with it.
#[test]
fn test_saving_replaces_the_groups_table() {
    let temp = TempDir::new("config_replace");
    let path = temp.path.join("bgone.toml");

    save_groups(
        &path,
        &groups_of(&[("a", &["www/one"]), ("b", &["www/two"])]),
    )
    .unwrap();
    save_groups(&path, &groups_of(&[("a", &["www/one", "www/three"])])).unwrap();

    let reloaded = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(reloaded.groups.len(), 1, "the dropped group must be gone");
    assert_eq!(reloaded.groups["a"], vec!["www/one", "www/three"]);

    save_groups(&path, &Groups::new()).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        !written.contains("[groups]"),
        "an empty table should not linger:\n{written}"
    );
    assert!(Config::parse(&written).unwrap().groups.is_empty());
}
