mod code;
mod crates;
mod docs;
mod embed;
mod github;
mod hackage;
mod symbols;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ingest",
    about = "Index code / docs / GitHub into PostgreSQL for hybrid search"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Index a codebase and its markdown docs (incremental: unchanged files are skipped)
    Code {
        /// Path to the repository root
        repo_path: PathBuf,
        /// Clear existing chunks and re-index everything (also clears documents unless --no-docs)
        #[arg(long)]
        force: bool,
        /// Extra glob patterns to include (e.g. '**/*.roc')
        #[arg(short, long)]
        pattern: Vec<String>,
        /// Skip markdown doc indexing for this repo
        #[arg(long)]
        no_docs: bool,
    },
    /// Index markdown documentation only (AGENTS.md, README, .agent/**, and all .md files)
    Docs {
        /// Path to the repository root
        repo_path: PathBuf,
        /// Clear existing documents and re-index
        #[arg(long)]
        force: bool,
    },
    /// Index GitHub issues and pull requests
    Github {
        /// Repository in OWNER/REPO format
        repo: String,
        /// Re-fetch all items, ignoring watermarks
        #[arg(long)]
        force: bool,
        /// Streams to run: issues, prs, issue_comments, pr_comments (default: all)
        #[arg(long)]
        stream: Vec<String>,
    },
    /// Fetch a Haskell package from CHaP or Hackage and index it (checks CHaP first)
    Hackage {
        /// Package name (e.g. serialise, cardano-ledger-core)
        package: String,
        /// Package version (e.g. 0.2.6.1)
        version: String,
        /// Re-index even if already present
        #[arg(long)]
        force: bool,
    },
    /// Fetch a Rust crate from crates.io and index it
    Crate {
        /// Crate name (e.g. serde, tokio)
        package: String,
        /// Crate version (e.g. 1.0.0)
        version: String,
        /// Re-index even if already present
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let dsn =
        std::env::var("PG_DSN").unwrap_or_else(|_| "postgresql://127.0.0.1:5432/codebase".into());
    let pool = sqlx::PgPool::connect(&dsn).await?;

    let cli = Cli::parse();
    match cli.command {
        Commands::Code {
            repo_path,
            force,
            pattern,
            no_docs,
        } => {
            code::ingest_code(&pool, &repo_path, force, &pattern, None).await?;
            if !no_docs {
                docs::ingest_docs(&pool, &repo_path, force).await?;
            }
        }
        Commands::Docs { repo_path, force } => {
            docs::ingest_docs(&pool, &repo_path, force).await?;
        }
        Commands::Github {
            repo,
            force,
            stream,
        } => {
            github::ingest_github(&pool, &repo, force, &stream).await?;
        }
        Commands::Hackage {
            package,
            version,
            force,
        } => {
            hackage::ingest_hackage(&pool, &package, &version, force).await?;
        }
        Commands::Crate {
            package,
            version,
            force,
        } => {
            crates::ingest_crate(&pool, &package, &version, force).await?;
        }
    }
    Ok(())
}
