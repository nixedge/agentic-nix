use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{self, EnvFilter};

mod db;
mod embed;
mod fmt;
mod rerank;
mod tools;

#[derive(Parser)]
#[command(about = "Hybrid code search MCP server + CLI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List all indexed repositories with file and chunk counts
    Repos,
    /// Hybrid BM25+vector search over indexed code (same as MCP search_code)
    Search {
        query: String,
        #[arg(long, default_value = "10")]
        limit: i32,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
    /// BM25-only search over indexed code (best for exact identifiers)
    Bm25 {
        query: String,
        #[arg(long, default_value = "10")]
        limit: i32,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let dsn =
        std::env::var("PG_DSN").unwrap_or_else(|_| "postgresql://127.0.0.1:5432/codebase".into());

    match cli.command {
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();

            let pool = sqlx::PgPool::connect(&dsn).await?;
            let service = tools::CodeSearchServer::new(pool)
                .serve(stdio())
                .await
                .inspect_err(|e| tracing::error!("serving error: {:?}", e))?;
            service.waiting().await?;
        }

        Some(Command::Repos) => {
            let pool = sqlx::PgPool::connect(&dsn).await?;
            let repos = db::list_repos(&pool).await?;
            if repos.is_empty() {
                println!("No repositories indexed yet.\nRun: just index /path/to/repo");
            } else {
                for r in &repos {
                    println!(
                        "{}\n  {} files · {} chunks · last indexed {}",
                        r.repo_path, r.files, r.chunks, r.last_indexed
                    );
                }
            }
        }

        Some(Command::Search {
            query,
            limit,
            language,
            kind,
        }) => {
            let pool = sqlx::PgPool::connect(&dsn).await?;
            let query_vec = embed::embed(&query)
                .await
                .map_err(|e| anyhow::anyhow!("Embedding failed (is Ollama running?): {e}"))?;
            let rows = db::hybrid_search(
                &pool,
                &query,
                &query_vec,
                limit,
                language.as_deref(),
                kind.as_deref(),
            )
            .await?;
            println!("{}", fmt::fmt_chunks(&rows, None, "rrf"));
        }

        Some(Command::Bm25 {
            query,
            limit,
            language,
            kind,
        }) => {
            let pool = sqlx::PgPool::connect(&dsn).await?;
            let rows =
                db::bm25_search(&pool, &query, limit, language.as_deref(), kind.as_deref()).await?;
            println!("{}", fmt::fmt_chunks(&rows, None, "bm25"));
        }
    }

    Ok(())
}
