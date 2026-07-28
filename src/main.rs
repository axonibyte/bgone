mod db;
mod exporter;
mod graph;
mod indexer;
mod reader;
mod ui;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "bgone")]
#[command(about = "Fast reactive TUI tree configuration tool for FreeBSD ports")]
#[command(after_help = "\
KEYBINDINGS:
  e / E        Expand subtree under cursor / Expand all nodes
  c / C        Collapse subtree under cursor / Collapse all nodes
  Space        Toggle selected option or switch radio choice
  Enter        Toggle single node expansion
  /            Open search and filter options
  s            Save options to disk and exit
  q / Esc      Quit without saving
")]
#[command(args_conflicts_with_subcommands = true)]
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
    #[arg(short = 'r', long)]
    force_reset: bool,

    /// Warn instead of bailing out on missing/unmatched ports (unless 0 total ports match)
    #[arg(short = 'i', long)]
    ignore_missing: bool,

    /// Read target port origins/patterns from a file (space, tab, newline, or comma-delimited)
    #[arg(short = 'f', long, value_name = "FILE")]
    file: Option<PathBuf>,

    /// Target port origin(s) or glob pattern(s) (e.g. www/apache24 "www/py-*")
    #[arg(value_name = "ORIGIN")]
    origins: Vec<String>,
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

fn parse_ports_file(path: &Path) -> Result<Vec<String>> {
    let content = fs::read_to_string(path)?;
    let origins = content
        .lines()
        .map(|line| line.split('#').next().unwrap_or("")) // Strip comment portion
        .flat_map(|line| line.split(|c: char| c == ',' || c.is_whitespace()))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    Ok(origins)
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
            let mut targets = cli.origins.clone();

            if let Some(ref file_path) = cli.file {
                match parse_ports_file(file_path) {
                    Ok(mut file_targets) => targets.append(&mut file_targets),
                    Err(e) => {
                        eprintln!("[!] Error reading ports file '{:?}': {e}", file_path);
                        std::process::exit(1);
                    }
                }
            }

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
