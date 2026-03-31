use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::embed::{embed_batch, vec_literal};

const GITHUB_API: &str = "https://api.github.com";
const PAGE_SIZE: usize = 100;
const EMBED_BATCH: usize = 8;
const MIN_BODY_LEN: usize = 20;

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn ingest_github(
    pool: &PgPool,
    repo: &str,
    force: bool,
    streams: &[String],
) -> Result<()> {
    if !repo.contains('/') {
        anyhow::bail!("repo must be in OWNER/REPO format, got: {}", repo);
    }

    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if token.is_empty() {
        eprintln!("Warning: GITHUB_TOKEN not set. Rate limit: 60 req/hour.");
    }

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let do_all = streams.is_empty() || streams.iter().any(|s| s == "all");

    if do_all || streams.iter().any(|s| s == "issues") {
        eprintln!("── Issues ──");
        match ingest_issues(pool, &http, &token, repo, force).await {
            Ok((n, s)) => eprintln!("  ✓ {} upserted, {} skipped", n, s),
            Err(e) => {
                record_error(pool, "github", &format!("{repo}:issues"), &e.to_string()).await;
                eprintln!("  ✗ issues failed: {e}");
            }
        }
    }

    if do_all || streams.iter().any(|s| s == "prs") {
        eprintln!("── Pull Requests ──");
        match ingest_prs(pool, &http, &token, repo, force).await {
            Ok((n, s)) => eprintln!("  ✓ {} upserted, {} skipped", n, s),
            Err(e) => {
                record_error(pool, "github", &format!("{repo}:prs"), &e.to_string()).await;
                eprintln!("  ✗ PRs failed: {e}");
            }
        }
    }

    if do_all || streams.iter().any(|s| s == "issue_comments") {
        eprintln!("── Issue Comments ──");
        match ingest_issue_comments(pool, &http, &token, repo, force).await {
            Ok(n) => eprintln!("  ✓ {} upserted", n),
            Err(e) => {
                record_error(pool, "github", &format!("{repo}:issue_comments"), &e.to_string()).await;
                eprintln!("  ✗ issue comments failed: {e}");
            }
        }
    }

    if do_all || streams.iter().any(|s| s == "pr_comments") {
        eprintln!("── PR Review Comments ──");
        match ingest_pr_comments(pool, &http, &token, repo, force).await {
            Ok(n) => eprintln!("  ✓ {} upserted", n),
            Err(e) => {
                record_error(pool, "github", &format!("{repo}:pr_comments"), &e.to_string()).await;
                eprintln!("  ✗ PR review comments failed: {e}");
            }
        }
    }

    Ok(())
}

// ── GitHub API client helpers ─────────────────────────────────────────────────

fn gh_headers(token: &str) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
    headers.insert("X-GitHub-Api-Version", HeaderValue::from_static("2022-11-28"));
    if !token.is_empty() {
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(AUTHORIZATION, v);
        }
    }
    headers
}

/// Paginate a GitHub endpoint, yielding JSON arrays item by item.
async fn paginate(
    http: &reqwest::Client,
    token: &str,
    url: &str,
    extra_params: &[(&str, &str)],
) -> Result<Vec<serde_json::Value>> {
    let mut all = vec![];
    let mut next_url = Some(format!(
        "{url}?per_page={PAGE_SIZE}{}",
        extra_params
            .iter()
            .map(|(k, v)| format!("&{k}={v}"))
            .collect::<String>()
    ));
    let headers = gh_headers(token);

    while let Some(url) = next_url.take() {
        let resp = http
            .get(&url)
            .headers(headers.clone())
            .send()
            .await
            .context("GitHub API request failed")?;

        resp.error_for_status_ref()
            .with_context(|| format!("GitHub API error for {url}"))?;

        // Extract next page URL from Link header
        let link = resp
            .headers()
            .get("link")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        next_url = extract_next_link(&link);

        let data: serde_json::Value = resp.json().await?;
        if let Some(arr) = data.as_array() {
            all.extend(arr.iter().cloned());
        }
    }
    Ok(all)
}

fn extract_next_link(link_header: &str) -> Option<String> {
    for part in link_header.split(',') {
        let part = part.trim();
        if part.contains(r#"rel="next""#) {
            if let Some(url) = part.split(';').next() {
                let url = url.trim().trim_start_matches('<').trim_end_matches('>');
                return Some(url.to_string());
            }
        }
    }
    None
}

// ── Sync state helpers ────────────────────────────────────────────────────────

async fn get_watermark(pool: &PgPool, source: &str, scope: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT watermark FROM sync_state WHERE source_name = $1 AND scope_key = $2",
    )
    .bind(source)
    .bind(scope)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

async fn set_watermark(pool: &PgPool, source: &str, scope: &str, watermark: &str) {
    let id = format!("{source}:{scope}");
    let _ = sqlx::query(
        "INSERT INTO sync_state (id, source_name, scope_key, watermark, last_synced_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())
         ON CONFLICT (source_name, scope_key) DO UPDATE
             SET watermark      = EXCLUDED.watermark,
                 last_synced_at = NOW(),
                 updated_at     = NOW(),
                 error_count    = 0,
                 last_error     = NULL",
    )
    .bind(&id)
    .bind(source)
    .bind(scope)
    .bind(watermark)
    .execute(pool)
    .await;
}

async fn record_error(pool: &PgPool, source: &str, scope: &str, error: &str) {
    let id = format!("{source}:{scope}");
    let truncated = &error[..error.len().min(1000)];
    let _ = sqlx::query(
        "INSERT INTO sync_state (id, source_name, scope_key, error_count, last_error, updated_at)
         VALUES ($1, $2, $3, 1, $4, NOW())
         ON CONFLICT (source_name, scope_key) DO UPDATE
             SET error_count = sync_state.error_count + 1,
                 last_error  = EXCLUDED.last_error,
                 updated_at  = NOW()",
    )
    .bind(&id)
    .bind(source)
    .bind(scope)
    .bind(truncated)
    .execute(pool)
    .await;
}

// ── Content helpers ───────────────────────────────────────────────────────────

fn make_content(title: &str, body: Option<&str>) -> String {
    let title = title.trim();
    match body.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => format!("{title}\n\n{b}"),
        None => title.to_string(),
    }
}

fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

fn parse_ts(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn latest_ts<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(a), Some(b)) => {
            let ta = DateTime::parse_from_rfc3339(a).ok();
            let tb = DateTime::parse_from_rfc3339(b).ok();
            match (ta, tb) {
                (Some(ta), Some(tb)) => Some(if tb > ta { b } else { a }),
                _ => Some(a),
            }
        }
    }
}

fn labels_str(labels: &serde_json::Value) -> String {
    labels
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

// ── Issues ────────────────────────────────────────────────────────────────────

async fn ingest_issues(
    pool: &PgPool,
    http: &reqwest::Client,
    token: &str,
    repo: &str,
    force: bool,
) -> Result<(usize, usize)> {
    let scope = format!("{repo}:issues");
    let watermark = if force { None } else { get_watermark(pool, "github", &scope).await };

    let mut params = vec![("state", "all"), ("sort", "updated"), ("direction", "asc")];
    let since_owned: String;
    if let Some(ref wm) = watermark {
        eprintln!("  issues: incremental since {wm}");
        since_owned = wm.clone();
        params.push(("since", &since_owned));
    } else {
        eprintln!("  issues: full fetch");
    }

    let items = paginate(
        http,
        token,
        &format!("{GITHUB_API}/repos/{repo}/issues"),
        &params,
    )
    .await?;

    let mut new_watermark: Option<String> = watermark.clone();
    let mut upserted = 0usize;
    let mut skipped = 0usize;
    let mut pending: Vec<serde_json::Value> = vec![];

    for issue in items {
        // GitHub issues API returns PRs too — filter them out
        if issue["pull_request"].is_object() {
            continue;
        }
        new_watermark = latest_ts(
            new_watermark.as_deref(),
            issue["updated_at"].as_str(),
        )
        .map(str::to_string);

        pending.push(issue);
        if pending.len() >= EMBED_BATCH {
            flush_issues(&mut pending, pool, repo, &mut upserted, &mut skipped).await?;
        }
    }
    flush_issues(&mut pending, pool, repo, &mut upserted, &mut skipped).await?;

    if let Some(wm) = &new_watermark {
        set_watermark(pool, "github", &scope, wm).await;
    }
    Ok((upserted, skipped))
}

async fn flush_issues(
    pending: &mut Vec<serde_json::Value>,
    pool: &PgPool,
    repo: &str,
    upserted: &mut usize,
    _skipped: &mut usize,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let contents: Vec<String> = pending
        .iter()
        .map(|issue| {
            make_content(
                issue["title"].as_str().unwrap_or(""),
                issue["body"].as_str(),
            )
        })
        .collect();
    let texts: Vec<&str> = contents.iter().map(String::as_str).collect();
    let embeddings = embed_batch(&texts).await?;

    for (issue, emb) in pending.iter().zip(embeddings.iter()) {
        let content = make_content(
            issue["title"].as_str().unwrap_or(""),
            issue["body"].as_str(),
        );
        let hash = content_hash(&content);
        sqlx::query(
            "INSERT INTO github_issues
                 (repo, number, state, title, body, author, labels, content,
                  created_at, updated_at, closed_at, content_hash, embedding)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13::vector)
             ON CONFLICT (repo, number) DO UPDATE
                 SET state        = EXCLUDED.state,
                     title        = EXCLUDED.title,
                     body         = EXCLUDED.body,
                     author       = EXCLUDED.author,
                     labels       = EXCLUDED.labels,
                     content      = EXCLUDED.content,
                     updated_at   = EXCLUDED.updated_at,
                     closed_at    = EXCLUDED.closed_at,
                     content_hash = EXCLUDED.content_hash,
                     embedding    = EXCLUDED.embedding,
                     indexed_at   = NOW()
                 WHERE github_issues.content_hash IS DISTINCT FROM EXCLUDED.content_hash",
        )
        .bind(repo)
        .bind(issue["number"].as_i64().unwrap_or(0) as i32)
        .bind(issue["state"].as_str())
        .bind(issue["title"].as_str().unwrap_or(""))
        .bind(issue["body"].as_str().unwrap_or(""))
        .bind(issue["user"]["login"].as_str())
        .bind(labels_str(&issue["labels"]))
        .bind(&content)
        .bind(parse_ts(issue["created_at"].as_str()))
        .bind(parse_ts(issue["updated_at"].as_str()))
        .bind(parse_ts(issue["closed_at"].as_str()))
        .bind(&hash)
        .bind(vec_literal(emb))
        .execute(pool)
        .await?;
        *upserted += 1;
    }
    pending.clear();
    Ok(())
}

// ── Pull Requests ─────────────────────────────────────────────────────────────

async fn ingest_prs(
    pool: &PgPool,
    http: &reqwest::Client,
    token: &str,
    repo: &str,
    force: bool,
) -> Result<(usize, usize)> {
    let scope = format!("{repo}:prs");
    let watermark = if force { None } else { get_watermark(pool, "github", &scope).await };

    let params = [("state", "all"), ("sort", "updated"), ("direction", "asc")];
    if let Some(ref wm) = watermark {
        eprintln!("  PRs: incremental since {wm}");
    } else {
        eprintln!("  PRs: full fetch");
    }

    let watermark_dt = watermark.as_deref().and_then(|s| {
        DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
    });

    let items = paginate(
        http,
        token,
        &format!("{GITHUB_API}/repos/{repo}/pulls"),
        &params,
    )
    .await?;

    let mut new_watermark: Option<String> = watermark.clone();
    let mut upserted = 0usize;
    let mut skipped = 0usize;
    let mut pending: Vec<serde_json::Value> = vec![];

    for pr in items {
        // GitHub PRs API has no `since` param — filter client-side
        if let Some(wm_dt) = watermark_dt {
            if let Some(updated_dt) = parse_ts(pr["updated_at"].as_str()) {
                if updated_dt <= wm_dt {
                    skipped += 1;
                    continue;
                }
            }
        }
        new_watermark = latest_ts(
            new_watermark.as_deref(),
            pr["updated_at"].as_str(),
        )
        .map(str::to_string);

        pending.push(pr);
        if pending.len() >= EMBED_BATCH {
            flush_prs(&mut pending, pool, repo, &mut upserted).await?;
        }
    }
    flush_prs(&mut pending, pool, repo, &mut upserted).await?;

    if let Some(wm) = &new_watermark {
        set_watermark(pool, "github", &scope, wm).await;
    }
    Ok((upserted, skipped))
}

async fn flush_prs(
    pending: &mut Vec<serde_json::Value>,
    pool: &PgPool,
    repo: &str,
    upserted: &mut usize,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let contents: Vec<String> = pending
        .iter()
        .map(|pr| {
            make_content(
                pr["title"].as_str().unwrap_or(""),
                pr["body"].as_str(),
            )
        })
        .collect();
    let texts: Vec<&str> = contents.iter().map(String::as_str).collect();
    let embeddings = embed_batch(&texts).await?;

    for (pr, emb) in pending.iter().zip(embeddings.iter()) {
        let content = make_content(
            pr["title"].as_str().unwrap_or(""),
            pr["body"].as_str(),
        );
        let hash = content_hash(&content);
        let state = if pr["merged_at"].is_string() {
            "merged"
        } else {
            pr["state"].as_str().unwrap_or("open")
        };
        sqlx::query(
            "INSERT INTO github_prs
                 (repo, number, state, title, body, author, labels,
                  base_branch, head_branch, content,
                  created_at, updated_at, merged_at, content_hash, embedding)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::vector)
             ON CONFLICT (repo, number) DO UPDATE
                 SET state        = EXCLUDED.state,
                     title        = EXCLUDED.title,
                     body         = EXCLUDED.body,
                     author       = EXCLUDED.author,
                     labels       = EXCLUDED.labels,
                     content      = EXCLUDED.content,
                     updated_at   = EXCLUDED.updated_at,
                     merged_at    = EXCLUDED.merged_at,
                     content_hash = EXCLUDED.content_hash,
                     embedding    = EXCLUDED.embedding,
                     indexed_at   = NOW()
                 WHERE github_prs.content_hash IS DISTINCT FROM EXCLUDED.content_hash",
        )
        .bind(repo)
        .bind(pr["number"].as_i64().unwrap_or(0) as i32)
        .bind(state)
        .bind(pr["title"].as_str().unwrap_or(""))
        .bind(pr["body"].as_str().unwrap_or(""))
        .bind(pr["user"]["login"].as_str())
        .bind(labels_str(&pr["labels"]))
        .bind(pr["base"]["ref"].as_str())
        .bind(pr["head"]["ref"].as_str())
        .bind(&content)
        .bind(parse_ts(pr["created_at"].as_str()))
        .bind(parse_ts(pr["updated_at"].as_str()))
        .bind(parse_ts(pr["merged_at"].as_str()))
        .bind(&hash)
        .bind(vec_literal(emb))
        .execute(pool)
        .await?;
        *upserted += 1;
    }
    pending.clear();
    Ok(())
}

// ── Issue comments ────────────────────────────────────────────────────────────

async fn ingest_issue_comments(
    pool: &PgPool,
    http: &reqwest::Client,
    token: &str,
    repo: &str,
    force: bool,
) -> Result<usize> {
    let scope = format!("{repo}:issue_comments");
    let watermark = if force { None } else { get_watermark(pool, "github", &scope).await };

    let mut params = vec![("sort", "updated"), ("direction", "asc")];
    let since_owned: String;
    if let Some(ref wm) = watermark {
        eprintln!("  issue comments: incremental since {wm}");
        since_owned = wm.clone();
        params.push(("since", &since_owned));
    } else {
        eprintln!("  issue comments: full fetch");
    }

    let items = paginate(
        http,
        token,
        &format!("{GITHUB_API}/repos/{repo}/issues/comments"),
        &params,
    )
    .await?;

    let mut new_watermark: Option<String> = watermark.clone();
    let mut upserted = 0usize;
    let mut pending: Vec<serde_json::Value> = vec![];

    for comment in items {
        let body = comment["body"].as_str().unwrap_or("");
        if body.trim().len() < MIN_BODY_LEN {
            continue;
        }
        // Extract issue number from issue_url: .../issues/123
        let issue_number: Option<i32> = comment["issue_url"]
            .as_str()
            .and_then(|u| u.rsplit('/').next())
            .and_then(|s| s.parse().ok());
        if issue_number.is_none() {
            continue;
        }
        new_watermark = latest_ts(
            new_watermark.as_deref(),
            comment["updated_at"].as_str(),
        )
        .map(str::to_string);

        pending.push(comment);
        if pending.len() >= EMBED_BATCH {
            flush_issue_comments(&mut pending, pool, repo, &mut upserted).await?;
        }
    }
    flush_issue_comments(&mut pending, pool, repo, &mut upserted).await?;

    if let Some(wm) = &new_watermark {
        set_watermark(pool, "github", &scope, wm).await;
    }
    Ok(upserted)
}

async fn flush_issue_comments(
    pending: &mut Vec<serde_json::Value>,
    pool: &PgPool,
    repo: &str,
    upserted: &mut usize,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let texts: Vec<&str> = pending
        .iter()
        .map(|c| c["body"].as_str().unwrap_or(""))
        .collect();
    let embeddings = embed_batch(&texts).await?;

    for (comment, emb) in pending.iter().zip(embeddings.iter()) {
        let body = comment["body"].as_str().unwrap_or("");
        let issue_number: i32 = comment["issue_url"]
            .as_str()
            .and_then(|u| u.rsplit('/').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let hash = content_hash(body);

        sqlx::query(
            "INSERT INTO github_issue_comments
                 (repo, issue_number, comment_id, author, content,
                  created_at, updated_at, content_hash, embedding)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9::vector)
             ON CONFLICT (repo, comment_id) DO UPDATE
                 SET author       = EXCLUDED.author,
                     content      = EXCLUDED.content,
                     updated_at   = EXCLUDED.updated_at,
                     content_hash = EXCLUDED.content_hash,
                     embedding    = EXCLUDED.embedding,
                     indexed_at   = NOW()
                 WHERE github_issue_comments.content_hash IS DISTINCT FROM EXCLUDED.content_hash",
        )
        .bind(repo)
        .bind(issue_number)
        .bind(comment["id"].as_i64().unwrap_or(0))
        .bind(comment["user"]["login"].as_str())
        .bind(body)
        .bind(parse_ts(comment["created_at"].as_str()))
        .bind(parse_ts(comment["updated_at"].as_str()))
        .bind(&hash)
        .bind(vec_literal(emb))
        .execute(pool)
        .await?;
        *upserted += 1;
    }
    pending.clear();
    Ok(())
}

// ── PR review comments ────────────────────────────────────────────────────────

async fn ingest_pr_comments(
    pool: &PgPool,
    http: &reqwest::Client,
    token: &str,
    repo: &str,
    force: bool,
) -> Result<usize> {
    let scope = format!("{repo}:pr_comments");
    let watermark = if force { None } else { get_watermark(pool, "github", &scope).await };

    let mut params = vec![("sort", "updated"), ("direction", "asc")];
    let since_owned: String;
    if let Some(ref wm) = watermark {
        eprintln!("  PR review comments: incremental since {wm}");
        since_owned = wm.clone();
        params.push(("since", &since_owned));
    } else {
        eprintln!("  PR review comments: full fetch");
    }

    let items = paginate(
        http,
        token,
        &format!("{GITHUB_API}/repos/{repo}/pulls/comments"),
        &params,
    )
    .await?;

    let mut new_watermark: Option<String> = watermark.clone();
    let mut upserted = 0usize;
    let mut pending: Vec<serde_json::Value> = vec![];

    for comment in items {
        let body = comment["body"].as_str().unwrap_or("");
        if body.trim().len() < MIN_BODY_LEN {
            continue;
        }
        let pr_number: Option<i32> = comment["pull_request_url"]
            .as_str()
            .and_then(|u| u.rsplit('/').next())
            .and_then(|s| s.parse().ok());
        if pr_number.is_none() {
            continue;
        }
        new_watermark = latest_ts(
            new_watermark.as_deref(),
            comment["updated_at"].as_str(),
        )
        .map(str::to_string);

        pending.push(comment);
        if pending.len() >= EMBED_BATCH {
            flush_pr_comments(&mut pending, pool, repo, &mut upserted).await?;
        }
    }
    flush_pr_comments(&mut pending, pool, repo, &mut upserted).await?;

    if let Some(wm) = &new_watermark {
        set_watermark(pool, "github", &scope, wm).await;
    }
    Ok(upserted)
}

async fn flush_pr_comments(
    pending: &mut Vec<serde_json::Value>,
    pool: &PgPool,
    repo: &str,
    upserted: &mut usize,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }

    // Build full searchable content: prepend file path if available
    let contents: Vec<String> = pending
        .iter()
        .map(|c| {
            let body = c["body"].as_str().unwrap_or("");
            match c["path"].as_str() {
                Some(p) => format!("{p}\n{body}"),
                None => body.to_string(),
            }
        })
        .collect();
    let texts: Vec<&str> = contents.iter().map(String::as_str).collect();
    let embeddings = embed_batch(&texts).await?;

    for (comment, emb) in pending.iter().zip(embeddings.iter()) {
        let body = comment["body"].as_str().unwrap_or("");
        let pr_number: i32 = comment["pull_request_url"]
            .as_str()
            .and_then(|u| u.rsplit('/').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let file_path = comment["path"].as_str();
        let content = match file_path {
            Some(p) => format!("{p}\n{body}"),
            None => body.to_string(),
        };
        let diff_hunk = comment["diff_hunk"].as_str().map(|s| &s[..s.len().min(2000)]);
        let hash = content_hash(&content);

        sqlx::query(
            "INSERT INTO github_pr_comments
                 (repo, pr_number, comment_id, comment_type, author, content,
                  file_path, diff_hunk, created_at, updated_at, content_hash, embedding)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12::vector)
             ON CONFLICT (repo, comment_type, comment_id) DO UPDATE
                 SET author       = EXCLUDED.author,
                     content      = EXCLUDED.content,
                     updated_at   = EXCLUDED.updated_at,
                     content_hash = EXCLUDED.content_hash,
                     embedding    = EXCLUDED.embedding,
                     indexed_at   = NOW()
                 WHERE github_pr_comments.content_hash IS DISTINCT FROM EXCLUDED.content_hash",
        )
        .bind(repo)
        .bind(pr_number)
        .bind(comment["id"].as_i64().unwrap_or(0))
        .bind("review_comment")
        .bind(comment["user"]["login"].as_str())
        .bind(&content)
        .bind(file_path)
        .bind(diff_hunk)
        .bind(parse_ts(comment["created_at"].as_str()))
        .bind(parse_ts(comment["updated_at"].as_str()))
        .bind(&hash)
        .bind(vec_literal(emb))
        .execute(pool)
        .await?;
        *upserted += 1;
    }
    pending.clear();
    Ok(())
}
