mod code;
mod crates;
mod docs;
mod embed;
mod git;
mod github;
mod hackage;
mod prune;
mod repo_index;
mod symbols;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use repo_index::{upsert_repo, RepoMeta};

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
    /// Index a local codebase and its markdown docs (incremental: unchanged files are skipped).
    /// The repo is marked dirty in repo_index since it is a local clone, not a released version.
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
    /// Clone a git repository by branch, tag, or commit hash and index it
    Git {
        /// Repository URL (https or ssh)
        url: String,
        /// Commit hash to check out (requires a full clone)
        #[arg(long)]
        rev: Option<String>,
        /// Branch name to clone (shallow)
        #[arg(long)]
        branch: Option<String>,
        /// Tag name to clone (shallow)
        #[arg(long)]
        tag: Option<String>,
        /// Re-index even if already present
        #[arg(long)]
        force: bool,
        /// Skip markdown doc indexing
        #[arg(long)]
        no_docs: bool,
    },
    /// List all indexed repos from repo_index
    List {
        /// Show only local clone repos
        #[arg(long)]
        local: bool,
        /// Filter by source kind: local, hackage, chap, crates.io, git
        #[arg(long)]
        source_kind: Option<String>,
    },
    /// Prune indexed repos from the database
    Prune {
        #[command(subcommand)]
        target: PruneTarget,
        /// Show what would be deleted without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum PruneTarget {
    /// Delete all local clone repos (source_kind = 'local')
    Local,
    /// For each package, delete all but the most recently indexed version
    OldVersions {
        /// Limit to a specific package name
        #[arg(long)]
        package: Option<String>,
    },
    /// Delete a specific repo by its repo_path key
    Repo {
        /// The repo_path identifier (e.g. 'hackage::serialise-0.2.6.1')
        repo_path: String,
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
            let canonical = repo_path.canonicalize()?;
            let repo_str = canonical.to_string_lossy().into_owned();
            code::ingest_code(&pool, &repo_path, force, &pattern, None).await?;
            if !no_docs {
                docs::ingest_docs(&pool, &repo_path, force, None).await?;
            }
            upsert_repo(
                &pool,
                &repo_str,
                &RepoMeta {
                    source_kind: "local",
                    package_name: None,
                    version: None,
                    git_url: None,
                    git_rev: None,
                },
            )
            .await?;
        }
        Commands::Docs { repo_path, force } => {
            docs::ingest_docs(&pool, &repo_path, force, None).await?;
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
        Commands::Git {
            url,
            rev,
            branch,
            tag,
            force,
            no_docs,
        } => {
            git::ingest_git(
                &pool,
                &url,
                rev.as_deref(),
                branch.as_deref(),
                tag.as_deref(),
                force,
                no_docs,
            )
            .await?;
        }
        Commands::List { local, source_kind } => {
            list_repos(&pool, local, source_kind.as_deref()).await?;
        }
        Commands::Prune { target, dry_run } => match target {
            PruneTarget::Local => {
                prune::prune_dirty(&pool, dry_run).await?;
            }
            PruneTarget::OldVersions { package } => {
                prune::prune_old_versions(&pool, package.as_deref(), dry_run).await?;
            }
            PruneTarget::Repo { repo_path } => {
                prune::prune_repo(&pool, &repo_path, dry_run).await?;
            }
        },
    }
    Ok(())
}

async fn list_repos(pool: &sqlx::PgPool, local: bool, source_kind: Option<&str>) -> Result<()> {
    let rows: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        r#"
        SELECT repo_path, source_kind, package_name, version
        FROM repo_index
        WHERE ($1 = false OR source_kind = 'local')
          AND ($2::text IS NULL OR source_kind = $2)
        ORDER BY indexed_at DESC
        "#,
    )
    .bind(local)
    .bind(source_kind)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        eprintln!("No repos in index.");
        return Ok(());
    }

    println!(
        "{:<55} {:<10} {:<20} {}",
        "repo_path", "kind", "package", "version"
    );
    println!("{}", "-".repeat(100));
    for (repo_path, kind, package_name, version) in &rows {
        println!(
            "{:<55} {:<10} {:<20} {}",
            repo_path,
            kind,
            package_name.as_deref().unwrap_or("-"),
            version.as_deref().unwrap_or("-"),
        );
    }
    println!("\n{} repo(s) listed.", rows.len());
    Ok(())
}
