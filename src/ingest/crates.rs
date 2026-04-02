use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sqlx::PgPool;
use std::io::Cursor;
use tar::Archive;
use tempfile::TempDir;

use super::code::ingest_code;
use super::repo_index::{upsert_repo, RepoMeta};

const CRATES_IO_BASE: &str = "https://static.crates.io/crates";

pub async fn ingest_crate(pool: &PgPool, package: &str, version: &str, force: bool) -> Result<()> {
    let pkg_ver = format!("{package}-{version}");
    let repo_path_str = format!("crates.io::{pkg_ver}");

    if !force {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1")
                .bind(&repo_path_str)
                .fetch_one(pool)
                .await
                .unwrap_or(0);

        if count > 0 {
            eprintln!("Crate {pkg_ver} already indexed ({count} chunks). Use --force to re-index.");
            return Ok(());
        }
    }

    let url = format!("{CRATES_IO_BASE}/{package}/{pkg_ver}.crate");
    eprintln!("Fetching {url}");

    // crates.io requires a descriptive User-Agent.
    let client = reqwest::Client::builder()
        .user_agent("agentic-nix (https://github.com/user/agentic-nix)")
        .build()
        .context("Failed to build HTTP client")?;

    let resp = client
        .get(&url)
        .send()
        .await
        .context("crates.io request failed")?;

    if !resp.status().is_success() {
        bail!("Crate {pkg_ver} not found on crates.io ({})", resp.status());
    }

    let bytes = resp
        .bytes()
        .await
        .context("Failed to read crates.io response body")?;
    eprintln!("Fetched {pkg_ver} ({} bytes). Extracting...", bytes.len());

    let tmp = TempDir::new().context("Failed to create temp directory")?;

    // .crate files are gzipped tarballs, identical format to .tar.gz.
    let gz = GzDecoder::new(Cursor::new(bytes.as_ref()));
    let mut archive = Archive::new(gz);
    archive
        .unpack(tmp.path())
        .context("Failed to extract .crate tarball")?;

    // Tarballs extract to a single top-level {name}-{version}/ directory.
    let expected = tmp.path().join(&pkg_ver);
    let ingest_dir = if expected.is_dir() {
        expected
    } else {
        std::fs::read_dir(tmp.path())
            .context("Failed to read temp directory")?
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .map(|e| e.path())
            .unwrap_or_else(|| tmp.path().to_path_buf())
    };

    ingest_code(pool, &ingest_dir, force, &[], Some(&repo_path_str)).await?;

    upsert_repo(
        pool,
        &repo_path_str,
        &RepoMeta {
            source_kind: "crates.io",
            package_name: Some(package),
            version: Some(version),
            git_url: None,
            git_rev: None,
        },
    )
    .await?;
    Ok(())
}
