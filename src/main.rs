mod cli;
mod db;
mod describe;
mod exporter;
mod graph;
mod indexer;
mod reader;
mod ui;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use cli::{Cli, Commands};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Instant;

/// Asks the ports tree about the ports being configured, so their option lists
/// and package names come from the tree rather than from a regex.
///
/// Best effort: without a readable tree bgone still works, it just keeps the
/// indexed approximation, which can leave `poudriere` prompting for ports whose
/// options the Makefile parse could not see.
fn describe_working_set(conn: &mut Connection, dep_graph: &graph::DependencyGraph) {
    let ports_dir = match db::get_meta(conn, "ports_dir").map(PathBuf::from) {
        Some(dir) if dir.is_dir() => dir,
        Some(dir) => {
            eprintln!(
                "[!] Ports tree {:?} is gone; using indexed data only. Re-run 'bgone index'.",
                dir
            );
            return;
        }
        None => {
            eprintln!("[!] No ports tree recorded; using indexed data only. Re-run 'bgone index'.");
            return;
        }
    };

    let origins: Vec<String> = dep_graph.ports.iter().map(|p| p.origin.clone()).collect();
    println!(
        "[*] Reading details for {} ports from the tree...",
        origins.len()
    );
    let start = Instant::now();

    match describe::describe_ports(conn, &ports_dir, &origins) {
        Ok(stats) => println!(
            "[+] {} read, {} already current, {} unavailable in {:.2?}",
            stats.described,
            stats.cached,
            stats.failed,
            start.elapsed()
        ),
        Err(e) => eprintln!("[!] Could not read port details: {e}"),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut conn = Connection::open(&cli.db_path)?;

    let force_reset =
        cli.force_reset || matches!(&cli.command, Some(Commands::Index { force: true, .. }));
    db::init_db(&conn, force_reset)?;

    match &cli.command {
        Some(Commands::Index { ports_dir, .. }) => {
            println!("[*] Indexing ports tree at {:?}...", ports_dir);
            let start = Instant::now();

            let stats = indexer::index_ports_dir(&mut conn, ports_dir)?;

            // Remembered so later runs can find the tree again to read port
            // details from, without having to be told where it is twice
            if let Ok(canonical) = ports_dir.canonicalize() {
                db::set_meta(&conn, "ports_dir", &canonical.to_string_lossy())?;
            }

            println!(
                "[+] Indexed {} ports, {} options, {} option dependencies, and {} unconditional dependencies in {:.2?}",
                stats.ports_indexed,
                stats.options_indexed,
                stats.option_deps_indexed,
                stats.port_deps_indexed,
                start.elapsed()
            );
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

                // Loaded once from the indexed data to find out which ports are
                // in play, then again once the tree has been asked about them.
                // Reading details cannot change which ports are reachable, only
                // what is known about them, so the second load sees the same set.
                let mut dep_graph = load(&conn);
                if !cli.no_describe {
                    describe_working_set(&mut conn, &dep_graph);
                    dep_graph = load(&conn);
                }

                let action = ui::run_tui(&mut dep_graph)?;

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
