// The modules live in the library crate; the binary is a thin front end over
// it. Declaring them here as well would compile everything twice and report
// anything only the tests use as dead code.
use bgone::{cli, db, exporter, graph, indexer, reader, resolve, ui};

use anyhow::Result;
use clap::CommandFactory;
use cli::{Cli, Commands};
use rusqlite::Connection;
use std::time::Instant;

fn main() -> Result<()> {
    let (cli, config) = cli::parse_with_config()?;
    let mut conn = Connection::open(&cli.db_path)?;

    let force_reset =
        cli.force_reset || matches!(&cli.command, Some(Commands::Index { force: true, .. }));
    db::init_db(&conn, force_reset)?;

    match &cli.command {
        Some(Commands::Index {
            ports_dir,
            jail_arch,
            osversion,
            opsys,
            osrel,
            ..
        }) => {
            let canonical = ports_dir
                .canonicalize()
                .unwrap_or_else(|_| ports_dir.clone());
            let mut env = resolve::MakeEnv::new(&canonical);
            env.arch = jail_arch.clone();
            env.osversion = osversion.clone();
            env.opsys = opsys.clone();
            env.osrel = osrel.clone();

            println!(
                "[*] Indexing ports tree at {:?} as {}...",
                canonical,
                env.describe_target()
            );
            println!("[*] Every port is evaluated by make; this is the slow part.");
            let start = Instant::now();

            let stats = indexer::index_ports_dir(&mut conn, &env)?;

            println!(
                "[+] Indexed {} ports ({} unchanged), {} options and {} dependency edges in {:.2?}",
                stats.ports_indexed,
                stats.cached,
                stats.options_indexed,
                stats.edges_indexed,
                start.elapsed()
            );
            if stats.failed > 0 {
                // Only the first few print their make error, because a tree
                // make cannot read at all fails identically for every port in
                // it. Once failures are per-port they are worth looking at
                // individually, so say where the rest are rather than leaving
                // them as a count.
                println!(
                    "[!] {} ports could not be evaluated; they will be retried on the next index.",
                    stats.failed
                );
                println!(
                    "[!]   sqlite3 {} \"SELECT port_origin FROM unresolved_dep WHERE reason='EVAL_FAILED'\"",
                    cli.db_path.display()
                );
            }
            if stats.unresolved > 0 {
                println!(
                    "[!] {} dependency entries resolved to nothing; see the unresolved_dep table",
                    stats.unresolved
                );
            }
        }
        None => {
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

            if !targets.is_empty() {
                println!("[*] Loading existing system options...");
                let sys_opts =
                    reader::SystemOptions::load(&cli.options_dir, cli.make_conf.as_deref());

                let load = |conn: &Connection| match graph::DependencyGraph::load_from_db(
                    conn,
                    &targets,
                    &sys_opts,
                    cli.ignore_missing,
                ) {
                    Ok(g) => g,
                    Err(e) => {
                        eprintln!("[!] Error: {e}");
                        std::process::exit(1);
                    }
                };

                // One load. The cache already holds what the tree says, because
                // `bgone index` evaluated every port with make; nothing here
                // needs to go back to the ports tree, or even have one.
                let mut dep_graph = load(&conn);
                // A port make could not read contributes no options, so nothing
                // is written for it and poudriere will still prompt. Said once,
                // up front, rather than left to be noticed as an empty port.
                let unevaluated = dep_graph.unevaluated_ports();
                if !unevaluated.is_empty() {
                    eprintln!(
                        "[!] {} port(s) could not be evaluated when the cache was built, so their \
                         options are unknown: {}{}",
                        unevaluated.len(),
                        unevaluated
                            .iter()
                            .take(3)
                            .copied()
                            .collect::<Vec<_>>()
                            .join(", "),
                        if unevaluated.len() > 3 { ", ..." } else { "" }
                    );
                    eprintln!(
                        "[!] Re-run 'bgone index' with a readable ports tree to fill them in."
                    );
                }

                if let Some(config) = &config {
                    dep_graph.groups = config.groups.clone();
                }

                let action = ui::run_tui(&mut dep_graph, cli.config.clone())?;

                match action {
                    ui::TuiAction::SaveAndQuit => {
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
            } else {
                Cli::command().print_help()?;
                println!();
            }
        }
    }

    Ok(())
}
