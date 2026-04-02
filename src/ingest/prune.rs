use anyhow::Result;
use sqlx::PgPool;

/// Delete all data for a single repo_path and remove it from repo_index.
pub async fn prune_repo(pool: &PgPool, repo_path: &str, dry_run: bool) -> Result<()> {
    let chunks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1")
            .bind(repo_path)
            .fetch_one(pool)
            .await
            .unwrap_or(0);
    let docs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE repo_path = $1")
        .bind(repo_path)
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    if dry_run {
        eprintln!("  [dry-run] {repo_path}: {chunks} chunks, {docs} docs");
    } else {
        sqlx::query("DELETE FROM code_chunks WHERE repo_path = $1")
            .bind(repo_path)
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM documents WHERE repo_path = $1")
            .bind(repo_path)
            .execute(pool)
            .await?;
        sqlx::query("DELETE FROM repo_index WHERE repo_path = $1")
            .bind(repo_path)
            .execute(pool)
            .await?;
        eprintln!("  Pruned {repo_path}: {chunks} chunks, {docs} docs deleted.");
    }
    Ok(())
}

/// Prune all repos marked as dirty (local clones).
pub async fn prune_dirty(pool: &PgPool, dry_run: bool) -> Result<()> {
    let dirty: Vec<String> =
        sqlx::query_scalar("SELECT repo_path FROM repo_index WHERE source_kind = 'local' ORDER BY repo_path")
            .fetch_all(pool)
            .await?;

    if dirty.is_empty() {
        eprintln!("No local repos found.");
        return Ok(());
    }

    eprintln!("Found {} local repo(s):", dirty.len());
    for repo_path in &dirty {
        prune_repo(pool, repo_path, dry_run).await?;
    }
    Ok(())
}

/// For each (source_kind, package_name), prune all but the most recently indexed version.
/// Optionally filter to a specific package name.
pub async fn prune_old_versions(
    pool: &PgPool,
    package_filter: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let old: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT ri.repo_path
        FROM repo_index ri
        WHERE ri.package_name IS NOT NULL
          AND ($1::text IS NULL OR ri.package_name = $1)
          AND ri.indexed_at < (
              SELECT MAX(ri2.indexed_at)
              FROM repo_index ri2
              WHERE ri2.source_kind = ri.source_kind
                AND ri2.package_name = ri.package_name
          )
        ORDER BY ri.repo_path
        "#,
    )
    .bind(package_filter)
    .fetch_all(pool)
    .await?;

    if old.is_empty() {
        eprintln!("No old versions found.");
        return Ok(());
    }

    eprintln!("Found {} old version(s):", old.len());
    for repo_path in &old {
        prune_repo(pool, repo_path, dry_run).await?;
    }
    Ok(())
}
