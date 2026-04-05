use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use sqlx::PgPool;
use std::io::Cursor;
use tar::Archive;
use tempfile::TempDir;
use zip::ZipArchive;

use super::code::ingest_code;
use super::repo_index::{upsert_repo, RepoMeta};

const PYPI_API_BASE: &str = "https://pypi.org/pypi";

#[derive(Deserialize)]
struct PypiResponse {
    urls: Vec<PypiUrl>,
}

#[derive(Deserialize)]
struct PypiUrl {
    packagetype: String,
    url: String,
    filename: String,
}

pub async fn ingest_pypi(pool: &PgPool, package: &str, version: &str, force: bool) -> Result<()> {
    let pkg_ver = format!("{package}-{version}");
    let repo_path_str = format!("pypi::{pkg_ver}");

    if !force {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1")
                .bind(&repo_path_str)
                .fetch_one(pool)
                .await
                .unwrap_or(0);

        if count > 0 {
            eprintln!("Package {pkg_ver} already indexed ({count} chunks). Use --force to re-index.");
            return Ok(());
        }
    }

    // Resolve the sdist download URL via the PyPI JSON API.
    let api_url = format!("{PYPI_API_BASE}/{package}/{version}/json");
    eprintln!("Fetching metadata from {api_url}");

    let client = reqwest::Client::builder()
        .user_agent("agentic-nix/0.1 (code search indexer; contact via GitHub)")
        .build()
        .context("Failed to build HTTP client")?;

    let meta: PypiResponse = client
        .get(&api_url)
        .send()
        .await
        .context("PyPI metadata request failed")?
        .error_for_status()
        .with_context(|| format!("Package {pkg_ver} not found on PyPI"))?
        .json()
        .await
        .context("Failed to parse PyPI metadata JSON")?;

    // Prefer sdist (.tar.gz) — it has the full source tree including tests/docs.
    // Fall back to a wheel (.whl) if no sdist is published (pure-Python wheels
    // are zip archives containing the .py files directly).
    let dist = meta
        .urls
        .iter()
        .find(|u| u.packagetype == "sdist")
        .or_else(|| meta.urls.iter().find(|u| u.filename.ends_with(".whl")))
        .or_else(|| meta.urls.first())
        .with_context(|| format!("No distributions found for {pkg_ver} on PyPI"))?;

    eprintln!("Downloading {} from {}", dist.filename, dist.url);
    let bytes = client
        .get(&dist.url)
        .send()
        .await
        .context("PyPI download request failed")?
        .error_for_status()
        .with_context(|| format!("Failed to download {}", dist.filename))?
        .bytes()
        .await
        .context("Failed to read PyPI download body")?;

    eprintln!("Downloaded {} ({} bytes). Extracting...", dist.filename, bytes.len());

    let tmp = TempDir::new().context("Failed to create temp directory")?;

    let ingest_dir = if dist.filename.ends_with(".tar.gz") {
        let gz = GzDecoder::new(Cursor::new(bytes.as_ref()));
        let mut archive = Archive::new(gz);
        archive
            .unpack(tmp.path())
            .context("Failed to extract PyPI tarball")?;

        // Sdists extract to a single top-level {name}-{version}/ directory.
        let expected = tmp.path().join(&pkg_ver);
        if expected.is_dir() {
            expected
        } else {
            std::fs::read_dir(tmp.path())
                .context("Failed to read temp directory")?
                .filter_map(|e| e.ok())
                .find(|e| e.path().is_dir())
                .map(|e| e.path())
                .unwrap_or_else(|| tmp.path().to_path_buf())
        }
    } else if dist.filename.ends_with(".whl") {
        // Wheels are zip archives. Files land at the top level of the zip
        // (the package directory is inside, not under a {name}-{version}/ wrapper).
        let mut zip = ZipArchive::new(Cursor::new(bytes.as_ref()))
            .context("Failed to open wheel zip archive")?;
        zip.extract(tmp.path())
            .context("Failed to extract wheel")?;
        // Index the whole temp dir — it contains the package tree directly.
        tmp.path().to_path_buf()
    } else {
        bail!(
            "Unsupported archive format '{}'. Expected .tar.gz or .whl.",
            dist.filename
        );
    };

    ingest_code(pool, &ingest_dir, force, &[], Some(&repo_path_str)).await?;

    upsert_repo(
        pool,
        &repo_path_str,
        &RepoMeta {
            source_kind: "pypi",
            package_name: Some(package),
            version: Some(version),
            git_url: None,
            git_rev: None,
        },
    )
    .await?;

    Ok(())
}
