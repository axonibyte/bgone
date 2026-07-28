mod db;
mod exporter;
mod graph;
mod indexer;
mod reader;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "bgone")]
#[command(about = "Fast reactive TUI tree configuration tool for FreeBSD ports", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to the SQLite cache database
    #[arg(short = 'd', long, default_value = "bgone_cache.db")]
    db_path: PathBuf,

    /// Directory to output FreeBSD option files
    #[arg(short, long, default_value = "/var/db/ports")]
    options_dir: PathBuf,

    /// Optional path to export/read a global make.conf snippet
    #[arg(short, long)]
    make_conf: Option<PathBuf>,

    /// Perform a dry-run without writing files to disk
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Discard previous database cache and rebuild schema
    #[arg(short = 'f', long)]
    force_reset: bool,

    /// Target port origin (e.g. www/apache24)
    origin: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
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
            if let Some(target) = cli.origin {
                println!("[*] Loading existing system options...");
                let sys_opts =
                    reader::SystemOptions::load(&cli.options_dir, cli.make_conf.as_deref());

                let mut dep_graph =
                    match graph::DependencyGraph::load_from_db(&conn, &target, &sys_opts) {
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
                println!("Usage: bgone <ORIGIN> [OPTIONS] or bgone index --ports-dir <PATH>");
            }
        }
    }

    Ok(())
}
