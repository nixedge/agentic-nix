use anyhow::{bail, Context, Result};
use sqlx::PgPool;
use std::path::Path;
use tempfile::TempDir;
use tokio::process::Command;

use super::code::ingest_code;
use super::docs::ingest_docs;
use super::repo_index::{upsert_repo, RepoMeta};

async fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(args);
    let output = cmd
        .output()
        .await
        .context("Failed to spawn git — is it on PATH?")?;
    if !output.status.success() {
        bail!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub async fn ingest_git(
    pool: &PgPool,
    url: &str,
    rev: Option<&str>,
    branch: Option<&str>,
    tag: Option<&str>,
    force: bool,
    no_docs: bool,
    project: Option<&str>,
) -> Result<()> {
    let tmp = TempDir::new().context("Failed to create temp directory")?;
    let tmp_str = tmp.path().to_str().expect("tempdir path is not valid UTF-8");

    // Clone the repository.
    if let Some(b) = branch.or(tag) {
        eprintln!("Cloning {url} (branch/tag: {b})...");
        run_git(
            &["clone", "--depth", "1", "--branch", b, url, tmp_str],
            None,
        )
        .await?;
    } else if let Some(r) = rev {
        // Arbitrary commit hashes require a full clone.
        eprintln!("Cloning {url} (rev: {r})...");
        run_git(&["clone", url, tmp_str], None).await?;
        run_git(&["checkout", r], Some(tmp.path())).await?;
    } else {
        eprintln!("Cloning {url} (HEAD)...");
        run_git(&["clone", "--depth", "1", url, tmp_str], None).await?;
    }

    // Resolve the exact commit hash.
    let resolved_rev = run_git(&["rev-parse", "HEAD"], Some(tmp.path())).await?;
    let short_rev = &resolved_rev[..12];
    let repo_path_str = format!("git::{url}@{short_rev}");

    // Check if already indexed.
    if !force {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1")
                .bind(&repo_path_str)
                .fetch_one(pool)
                .await
                .unwrap_or(0);
        if count > 0 {
            eprintln!(
                "Git repo {url}@{short_rev} already indexed ({count} chunks). Use --force to re-index."
            );
            return Ok(());
        }
    }

    ingest_code(pool, tmp.path(), force, &[], Some(&repo_path_str), project).await?;
    if !no_docs {
        ingest_docs(pool, tmp.path(), force, Some(&repo_path_str), project).await?;
    }

    upsert_repo(
        pool,
        &repo_path_str,
        &RepoMeta {
            source_kind: "git",
            package_name: None,
            version: None,
            git_url: Some(url),
            git_rev: Some(&resolved_rev),
            project,
        },
    )
    .await?;

    eprintln!("Indexed {repo_path_str}");
    Ok(())
}
