use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{self, EnvFilter};

mod db;
mod embed;
mod fmt;
mod rerank;
mod tools;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let dsn = std::env::var("PG_DSN")
        .unwrap_or_else(|_| "postgresql://127.0.0.1:5432/codebase".into());

    let pool = sqlx::PgPool::connect(&dsn).await?;

    let service = tools::CodeSearchServer::new(pool)
        .serve(stdio())
        .await
        .inspect_err(|e| {
            tracing::error!("serving error: {:?}", e);
        })?;

    service.waiting().await?;
    Ok(())
}
