use anyhow::Result;
use sqlx::PgPool;

pub struct RepoMeta<'a> {
    pub source_kind: &'a str,
    pub package_name: Option<&'a str>,
    pub version: Option<&'a str>,
    pub git_url: Option<&'a str>,
    pub git_rev: Option<&'a str>,
}

pub async fn upsert_repo(pool: &PgPool, repo_path: &str, meta: &RepoMeta<'_>) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO repo_index
            (repo_path, source_kind, package_name, version, git_url, git_rev, indexed_at)
        VALUES ($1, $2, $3, $4, $5, $6, NOW())
        ON CONFLICT (repo_path) DO UPDATE SET
            source_kind  = EXCLUDED.source_kind,
            package_name = EXCLUDED.package_name,
            version      = EXCLUDED.version,
            git_url      = EXCLUDED.git_url,
            git_rev      = EXCLUDED.git_rev,
            indexed_at   = NOW()
        "#,
    )
    .bind(repo_path)
    .bind(meta.source_kind)
    .bind(meta.package_name)
    .bind(meta.version)
    .bind(meta.git_url)
    .bind(meta.git_rev)
    .execute(pool)
    .await?;
    Ok(())
}
