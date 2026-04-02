use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sqlx::PgPool;
use std::io::Cursor;
use tar::Archive;
use tempfile::TempDir;

use super::code::ingest_code;

const CHAP_BASE: &str = "https://chap.intersectmbo.org/package";
const HACKAGE_BASE: &str = "https://hackage.haskell.org/package";

pub async fn ingest_hackage(
    pool: &PgPool,
    package: &str,
    version: &str,
    force: bool,
) -> Result<()> {
    let pkg_ver = format!("{package}-{version}");

    // Check if already indexed (unless force).
    if !force {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1 OR repo_path = $2",
        )
        .bind(format!("chap::{pkg_ver}"))
        .bind(format!("hackage::{pkg_ver}"))
        .fetch_one(pool)
        .await
        .unwrap_or(0);

        if count > 0 {
            eprintln!(
                "Package {pkg_ver} already indexed ({count} chunks). Use --force to re-index."
            );
            return Ok(());
        }
    }

    let (tarball_bytes, source) = download_tarball(package, version).await?;
    let repo_path_str = format!("{source}::{pkg_ver}");
    eprintln!("Fetched {pkg_ver} from {source}. Extracting...");

    let tmp = TempDir::new().context("Failed to create temp directory")?;
    extract_tarball(&tarball_bytes, tmp.path())
        .with_context(|| format!("Failed to extract {pkg_ver} tarball"))?;

    // Tarballs typically extract to a single top-level directory {name}-{version}/.
    // Fall back to the temp root if that directory doesn't exist.
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

    // tmp is dropped here, cleaning up the extracted files.
    Ok(())
}

async fn download_tarball(package: &str, version: &str) -> Result<(Vec<u8>, &'static str)> {
    let pkg_ver = format!("{package}-{version}");
    let client = reqwest::Client::new();

    // CHaP: flat URL scheme (no nested subdir).
    let chap_url = format!("{CHAP_BASE}/{pkg_ver}.tar.gz");
    eprintln!("Trying CHaP: {chap_url}");
    let resp = client
        .get(&chap_url)
        .send()
        .await
        .context("CHaP request failed")?;
    if resp.status().is_success() {
        let bytes = resp
            .bytes()
            .await
            .context("Failed to read CHaP response body")?;
        return Ok((bytes.to_vec(), "chap"));
    }
    let chap_status = resp.status();

    // Hackage: nested subdir scheme.
    let hackage_url = format!("{HACKAGE_BASE}/{pkg_ver}/{pkg_ver}.tar.gz");
    eprintln!("CHaP: {chap_status} — trying Hackage: {hackage_url}");
    let resp = client
        .get(&hackage_url)
        .send()
        .await
        .context("Hackage request failed")?;
    if resp.status().is_success() {
        let bytes = resp
            .bytes()
            .await
            .context("Failed to read Hackage response body")?;
        return Ok((bytes.to_vec(), "hackage"));
    }

    bail!(
        "Package {pkg_ver} not found on CHaP ({chap_status}) or Hackage ({})",
        resp.status()
    );
}

fn extract_tarball(bytes: &[u8], dest: &std::path::Path) -> Result<()> {
    let gz = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(gz);
    archive.unpack(dest)?;
    Ok(())
}
