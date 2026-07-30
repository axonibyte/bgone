mod cli;
mod db;
mod exporter;
mod graph;
mod indexer;
mod reader;
mod ui;

use anyhow::Result;
use clap::{CommandFactory, Parser};
use cli::{Cli, Commands};
use rusqlite::Connection;
use std::time::Instant;

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

            println!(
                "[+] Indexed {} ports, {} options, and {} option dependencies in {:.2?}",
                stats.ports_indexed,
                stats.options_indexed,
                stats.option_deps_indexed,
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

                let mut dep_graph = match graph::DependencyGraph::load_from_db(
                    &conn,
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
