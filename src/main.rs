use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use code_scope::{index, scan};

mod browse;
mod editor;
mod live;
mod lsp;
mod root;
mod watch;

#[derive(Parser)]
#[command(name = "scope", about = "Explore symbols in Rust and Go repositories")]
struct Cli {
    /// Indexing worker threads (default: Rayon's automatic selection).
    #[arg(short = 'j', long, global = true)]
    jobs: Option<NonZeroUsize>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Refresh symbols and open a match, or browse matches interactively.
    Open {
        #[arg(default_value = "")]
        query: String,
        /// Omit PATH to use the nearest Git root.
        path: Option<PathBuf>,
    },
    /// Serve workspace symbol search over the Language Server Protocol.
    Lsp {
        /// Omit PATH to use the nearest Git root, or the current directory.
        path: Option<PathBuf>,
    },
    /// Parse source files and print functions, methods, types, and traits.
    Scan {
        /// Repository, directory, or source file to scan.
        /// Omit PATH to use the nearest Git root, or the current directory.
        path: Option<PathBuf>,
    },
    /// Update a repository's saved symbols, parsing changed files only.
    Index {
        /// Repository to index.
        /// Omit PATH to use the nearest Git root, or the current directory.
        path: Option<PathBuf>,
    },
    /// Keep the saved index updated as repository files change.
    Watch {
        /// Repository to watch.
        /// Omit PATH to use the nearest Git root, or the current directory.
        path: Option<PathBuf>,
    },
    /// Search symbols interactively with a source preview.
    Browse {
        /// Repository to explore.
        /// Omit PATH to use the nearest Git root, or the current directory.
        path: Option<PathBuf>,
    },
    /// Search previously indexed symbol names without rescanning files.
    Find {
        /// Fuzzy query, optionally with kind:method and path:internal filters.
        query: String,
        /// Indexed repository.
        /// Omit PATH to use the nearest Git root, or the current directory.
        path: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let Cli { command, jobs } = Cli::parse();
    let explicit = match &command {
        Command::Open { path, .. }
        | Command::Lsp { path, .. }
        | Command::Scan { path, .. }
        | Command::Index { path, .. }
        | Command::Watch { path, .. }
        | Command::Browse { path, .. }
        | Command::Find { path, .. } => path.as_deref(),
    };
    let path = match root::resolve(explicit) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("scope: cannot determine repository root: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(jobs) = jobs
        && let Err(error) = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs.get())
            .build_global()
    {
        eprintln!("scope: cannot start indexing workers: {error}");
        return ExitCode::FAILURE;
    }
    match command {
        Command::Open { query, .. } => match editor::run(&path, &query) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("scope: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Lsp { .. } => match lsp::run(&path) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                eprintln!("scope: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Scan { .. } => match scan(&path) {
            Ok(report) => {
                for symbol in &report.symbols {
                    println!(
                        "{}:{}:{}\t{}\t{}",
                        symbol.path.display(),
                        symbol.line,
                        symbol.column,
                        symbol.kind,
                        symbol.display_name(),
                    );
                }
                for warning in &report.warnings {
                    eprintln!("warning: {}: {}", warning.path.display(), warning.message);
                }
                eprintln!(
                    "scanned {} files, found {} symbols, {} warnings",
                    report.files_parsed,
                    report.symbols.len(),
                    report.warnings.len()
                );
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("scope: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Index { .. } => match index::rebuild(&path) {
            Ok(report) => {
                print_index_report(&report);
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("scope: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Watch { .. } => match watch::run(&path, print_index_report) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("scope: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Browse { .. } => match browse::run(&path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("scope: {error}");
                ExitCode::FAILURE
            }
        },
        Command::Find { query, .. } => match index::find(&path, &query) {
            Ok(symbols) => {
                for symbol in &symbols {
                    println!(
                        "{}:{}:{}\t{}\t{}",
                        symbol.path.display(),
                        symbol.line,
                        symbol.column,
                        symbol.kind,
                        symbol.display_name(),
                    );
                }
                eprintln!("found {} symbols", symbols.len());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("scope: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn print_index_report(report: &index::IndexReport) {
    println!(
        "indexed {} symbols ({} parsed, {} unchanged, {} removed) at {}",
        report.symbols_indexed,
        report.files_parsed,
        report.files_unchanged,
        report.files_removed,
        report.database_path.display()
    );
}
