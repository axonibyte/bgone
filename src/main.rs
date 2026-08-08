// The modules live in the library crate; the binary is a thin front end over
// it. Declaring them here as well would compile everything twice and report
// anything only the tests use as dead code.
use bgone::{cli, db, exporter, graph, oracle, reader, resolve, ui};

use anyhow::Result;
use clap::CommandFactory;
use cli::{Cli, Commands};
use oracle::{Options, Oracle};
use rusqlite::Connection;
use std::time::Instant;

/// How the ports tree will be read: where it is, and what to resolve it as.
fn make_env(cli: &Cli) -> resolve::MakeEnv {
    let canonical = cli
        .ports_dir
        .canonicalize()
        .unwrap_or_else(|_| cli.ports_dir.clone());
    let mut env = resolve::MakeEnv::new(canonical);
    env.arch = cli.jail_arch.clone();
    env.osversion = cli.osversion.clone();
    env.opsys = cli.opsys.clone();
    env.osrel = cli.osrel.clone();
    env.via_jail = cli.poudriere_jail.clone();
    env
}

/// Fills the cache ahead of time. Optional — a configure run fills it as it
/// goes — but doing it up front means the session starts warm.
fn preheat(cli: &Cli, oracle: &Oracle, all: bool, origins: &[String]) -> Result<()> {
    let targets = if all {
        oracle.enumerate()?
    } else {
        let mut named = cli.collect_targets()?;
        named.extend(origins.iter().cloned());
        if named.is_empty() {
            anyhow::bail!(
                "Nothing to preheat. Name some ports, use -f, or pass --all for the whole tree."
            );
        }
        // Preheating what was *named* rather than the closure below it: the
        // closure depends on the options, and working it out means doing the
        // walk, which is the thing being preheated. The walk memoises as it
        // goes, so the second run pays only for what it reaches.
        named
    };

    println!(
        "[*] Preheating {} port(s) from {} as {}",
        targets.len(),
        oracle.ports_dir().display(),
        oracle.target()
    );
    let start = Instant::now();

    let want: Vec<_> = targets
        .iter()
        .map(|o| (o.clone(), Options::AsShipped))
        .collect();
    let answers = oracle.facts_many(&want);

    let failed: Vec<&str> = answers
        .iter()
        .filter(|(_, r)| r.is_err())
        .map(|(o, _)| o.as_str())
        .collect();

    println!(
        "[+] {} evaluated, {} could not be read, in {:.2?}",
        answers.len() - failed.len(),
        failed.len(),
        start.elapsed()
    );
    for origin in failed.iter().take(5) {
        if let Some((_, Err(e))) = answers.iter().find(|(o, _)| o == origin) {
            eprintln!("[!] {e}");
        }
    }
    if failed.len() > 5 {
        eprintln!("[!] ...and {} more", failed.len() - 5);
    }

    Ok(())
}

fn configure(cli: &Cli, config: Option<&bgone::config::Config>, oracle: &Oracle) -> Result<()> {
    let targets = match cli.collect_targets() {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "[!] Error reading ports file '{:?}': {e}",
                cli.file.as_deref().unwrap_or_else(|| "".as_ref())
            );
            std::process::exit(1);
        }
    };

    if targets.is_empty() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }

    // Said every run, because bgone cannot know which jail you are about to
    // build for — only you can spot that "host" is the wrong answer.
    println!(
        "[*] Ports tree: {} (resolved as {})",
        oracle.ports_dir().display(),
        oracle.target()
    );
    if !oracle.tree_readable() {
        eprintln!(
            "[!] No ports tree at {} - running from the cache alone. Ports it has never \
             seen cannot be resolved.",
            oracle.ports_dir().display()
        );
        eprintln!("[!] Point --ports-dir or --poudriere-ports at a readable tree.");
    }

    println!("[*] Loading existing system options...");
    let sys_opts = reader::SystemOptions::load(&cli.options_dir, cli.make_conf.as_deref());

    println!("[*] Resolving dependencies with make; the first run over a set is the slow part.");
    let start = Instant::now();
    let mut dep_graph =
        match graph::DependencyGraph::resolve(oracle, &targets, &sys_opts, cli.ignore_missing) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("[!] Error: {e}");
                std::process::exit(1);
            }
        };
    println!(
        "[+] {} ports in the build, resolved in {:.2?}",
        dep_graph.live_count(),
        start.elapsed()
    );

    // A port make could not read contributes no options, so nothing is written
    // for it and poudriere will still prompt. Said once, up front, rather than
    // left to be noticed as an empty port.
    let unevaluated = dep_graph.unevaluated_ports();
    if !unevaluated.is_empty() {
        eprintln!(
            "[!] {} port(s) could not be evaluated, so their options are unknown: {}{}",
            unevaluated.len(),
            unevaluated
                .iter()
                .take(3)
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
            if unevaluated.len() > 3 { ", ..." } else { "" }
        );
    }

    if let Some(config) = config {
        dep_graph.groups = config.groups.clone();
    }

    // What Ctrl + S calls. Writing from inside the interface means a long
    // session's picking is not all riding on getting out cleanly at the end.
    //
    // A dry run declines rather than printing: its output goes to stdout, which
    // the alternate screen is sitting on top of.
    let options_dir = cli.options_dir.clone();
    let make_conf = cli.make_conf.clone();
    let dry_run = cli.dry_run;
    let saver: ui::Saver = Box::new(move |graph: &graph::DependencyGraph| {
        if dry_run {
            return Ok(String::from("Dry run - nothing written"));
        }
        let stats = exporter::export_options(graph, &options_dir, false, make_conf.as_ref())?;
        Ok(format!(
            "Saved {} options across {} files",
            stats.options_saved, stats.files_written
        ))
    });

    // Ctrl+S writes mid-session, and the interface may still have questions out
    // from the last keystroke. Settling first is what keeps a mid-session save
    // from being a partial one.
    let settle_env = make_env(cli);
    let settle_db = cli.db_path.clone();
    let settle_opts = sys_opts.clone();
    let settle: ui::Settle = Box::new(move |graph: &mut graph::DependencyGraph| {
        let changed = graph.changed_ports();
        if changed.is_empty() {
            return graph::ResettleOutcome::default();
        }
        let oracle = Oracle::new(settle_env.clone(), settle_db.clone());
        graph.resettle(&oracle, &settle_opts, &changed)
    });

    // The interface keeps the oracle, because a toggle raises a question the
    // graph cannot answer from what it already holds.
    let action = ui::run_tui_with(
        &mut dep_graph,
        cli.config.clone(),
        saver,
        Some(settle),
        Some(Oracle::new(make_env(cli), cli.db_path.clone())),
        &sys_opts,
    )?;

    match action {
        ui::TuiAction::SaveAndQuit => {
            // The interface resolves off its event loop, so a question raised by
            // the last keystroke may still be out when it ends. What gets
            // written is exactly what is in the build, so the build has to be
            // settled first — otherwise a port pulled in by that last toggle is
            // silently left out, which is the failure this all exists to fix.
            let changed = dep_graph.changed_ports();
            if !changed.is_empty() {
                let oracle = Oracle::new(make_env(cli), cli.db_path.clone());
                let outcome = dep_graph.resettle(&oracle, &sys_opts, &changed);
                if !outcome.arrived.is_empty() {
                    println!(
                        "[*] {} port(s) pulled in by your changes: {}",
                        outcome.arrived.len(),
                        outcome.arrived.join(", ")
                    );
                }
                for (origin, why) in &outcome.failed {
                    eprintln!("[!] {origin} could not be re-evaluated: {why}");
                }
            }

            println!("[*] Exporting options...");
            let stats = exporter::export_options(
                &dep_graph,
                &cli.options_dir,
                cli.dry_run,
                cli.make_conf.as_ref(),
            )?;

            println!(
                "[+] Successfully processed {} options across {} files.",
                stats.options_saved, stats.files_written
            );
        }
        ui::TuiAction::QuitWithoutSaving => {
            println!("[!] Exited without saving changes.");
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    let (cli, config) = cli::parse_with_config()?;
    let conn = Connection::open(&cli.db_path)?;

    let force_reset =
        cli.force_reset || matches!(&cli.command, Some(Commands::Index { force: true, .. }));
    db::init_db(&conn, force_reset)?;
    drop(conn);

    let oracle = Oracle::new(make_env(&cli), cli.db_path.clone());

    match &cli.command {
        Some(Commands::Index { all, origins, .. }) => preheat(&cli, &oracle, *all, origins),
        None => configure(&cli, config.as_ref(), &oracle),
    }
}
