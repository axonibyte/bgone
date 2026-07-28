mod db;
mod graph;
mod indexer;
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

    /// Discard previous database cache and rebuild schema
    #[arg(short, long)]
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
    let mut conn = Connection::open("bgone_cache.db")?;

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
                let mut dep_graph = graph::DependencyGraph::load_from_db(&conn, &target)?;
                ui::run_tui(&mut dep_graph)?;
            } else {
                println!("Usage: bgone <ORIGIN> or bgone index --ports-dir <PATH> [--force]");
            }
        }
    }

    Ok(())
}
