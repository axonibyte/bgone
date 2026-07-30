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

use bgone::cli::{parse_ports_file, parse_ports_list, Cli, Commands};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(std::iter::once("bgone").chain(args.iter().copied()))
        .unwrap_or_else(|e| panic!("failed to parse {:?}: {e}", args))
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
        Some(Commands::Index { ports_dir, force }) => {
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
        "-d", "a.db", "-o", "b", "-m", "c", "-n", "-r", "-i", "-f", "d", "www/nginx",
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
            Some(Commands::Index { ports_dir, force }) => {
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
    ] {
        assert!(help.contains(binding), "`--help` is missing {binding}");
    }

    // Each expansion width must be described, not just listed
    for explanation in [
        "Just the row under the cursor",
        "That row and everything nested inside it",
        "The whole tree",
    ] {
        assert!(
            help.contains(explanation),
            "`--help` lists the expansion keys without explaining {explanation:?}"
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
            "INSERT INTO ports (origin, name, version, comment) VALUES ('stale/port', 's', '1', '')",
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

    let out = run(&[
        "-d",
        db.to_str().unwrap(),
        "-f",
        missing.to_str().unwrap(),
    ]);

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
