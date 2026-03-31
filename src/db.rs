use anyhow::Result;
use sqlx::{PgPool, Row as _};

// ── Code chunks ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ChunkRow {
    pub file_path: String,
    pub start_line: Option<i32>,
    pub end_line: Option<i32>,
    pub content: String,
    pub language: Option<String>,
    pub symbol_kind: Option<String>,
    pub rrf_score: Option<f64>,
}

pub struct RepoSummary {
    pub repo_path: String,
    pub chunks: i64,
    pub files: i64,
    pub last_indexed: String,
}

fn vec_literal(query_vec: &[f32]) -> String {
    format!(
        "[{}]",
        query_vec
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Hybrid BM25 + vector search over code_chunks with optional language / symbol_kind filters.
/// When filters are set, fetches 3× candidates then trims to `limit`.
pub async fn hybrid_search(
    pool: &PgPool,
    query_text: &str,
    query_vec: &[f32],
    limit: i32,
    language: Option<&str>,
    symbol_kind: Option<&str>,
) -> Result<Vec<ChunkRow>> {
    let vec_str = vec_literal(query_vec);
    // Over-fetch when filtering so we have enough after the WHERE clause prunes results
    let candidates = if language.is_some() || symbol_kind.is_some() {
        (limit * 3).max(20)
    } else {
        limit
    };

    let rows = sqlx::query(
        "SELECT h.file_path, h.start_line, h.end_line, h.content, h.language,
                c.symbol_kind, h.rrf_score
         FROM hybrid_search($1, $2::vector, $3) h
         JOIN code_chunks c USING (id)
         WHERE ($4 IS NULL OR h.language = $4)
           AND ($5 IS NULL OR c.symbol_kind = $5)
         ORDER BY h.rrf_score DESC
         LIMIT $6",
    )
    .bind(query_text)
    .bind(&vec_str)
    .bind(candidates)
    .bind(language)
    .bind(symbol_kind)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| ChunkRow {
            file_path: r.get("file_path"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
            content: r.get("content"),
            language: r.get("language"),
            symbol_kind: r.get("symbol_kind"),
            rrf_score: r.get("rrf_score"),
        })
        .collect())
}

/// Sanitize a BM25 query string for use with `paradedb.match`.
///
/// `paradedb.match` interprets several characters as query operators:
///   `-`  → NOT,  `+` → must,  `|` → OR,  `*` → wildcard,
///   `~`  → fuzzy, `^` → boost, `(` `)` → grouping, `"` → phrase
///
/// These are common in code identifiers (e.g. `cardano-lsm-rust`) and repo
/// names, so we replace them with spaces to get plain term matching.
fn sanitize_bm25_query(q: &str) -> String {
    q.chars()
        .map(|c| match c {
            '-' | '+' | '|' | '*' | '~' | '^' | '(' | ')' | '"' | '\\' => ' ',
            c => c,
        })
        .collect()
}

pub async fn bm25_search(
    pool: &PgPool,
    query: &str,
    limit: i32,
    language: Option<&str>,
    symbol_kind: Option<&str>,
) -> Result<Vec<ChunkRow>> {
    let query = sanitize_bm25_query(query);
    let query = query.trim();

    // Build SQL dynamically: ParadeDB raises "Unsupported query shape" when @@@ is
    // combined with conditional predicates like `($n IS NULL OR col = $n)` even
    // when the parameter is NULL.  Only append language/symbol_kind clauses when
    // the caller actually supplies a value.
    let mut sql = "SELECT file_path, start_line, end_line, content, language, symbol_kind,
                          paradedb.score(id)::float8 AS rrf_score
                   FROM   code_chunks
                   WHERE  id @@@ paradedb.match('content', $1)"
        .to_string();

    let mut next_param = 3usize; // $1 = query, $2 = limit
    if language.is_some() {
        sql.push_str(&format!(" AND language = ${next_param}"));
        next_param += 1;
    }
    if symbol_kind.is_some() {
        sql.push_str(&format!(" AND symbol_kind = ${next_param}"));
        next_param += 1;
    }
    let _ = next_param; // silence unused warning
    sql.push_str(" ORDER BY paradedb.score(id) DESC LIMIT $2");

    let q = sqlx::query(&sql).bind(query).bind(limit);
    let q = if let Some(lang) = language { q.bind(lang) } else { q };
    let q = if let Some(kind) = symbol_kind { q.bind(kind) } else { q };

    let rows = q.fetch_all(pool).await?;

    Ok(rows
        .iter()
        .map(|r| ChunkRow {
            file_path: r.get("file_path"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
            content: r.get("content"),
            language: r.get("language"),
            symbol_kind: r.get("symbol_kind"),
            rrf_score: r.get("rrf_score"),
        })
        .collect())
}

pub async fn list_repos(pool: &PgPool) -> Result<Vec<RepoSummary>> {
    let rows = sqlx::query(
        "SELECT repo_path,
                COUNT(*)                  AS chunks,
                COUNT(DISTINCT file_path) AS files,
                MAX(indexed_at)::TEXT     AS last_indexed
         FROM   code_chunks
         GROUP  BY repo_path
         ORDER  BY MAX(indexed_at) DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| RepoSummary {
            repo_path: r.get("repo_path"),
            chunks: r.get("chunks"),
            files: r.get("files"),
            last_indexed: r.get::<Option<String>, _>("last_indexed").unwrap_or_default(),
        })
        .collect())
}

pub async fn get_file_chunks(
    pool: &PgPool,
    repo_path: &str,
    file_path: &str,
) -> Result<Vec<ChunkRow>> {
    let rows = sqlx::query(
        "SELECT file_path, start_line, end_line, content, language, symbol_kind
         FROM   code_chunks
         WHERE  repo_path = $1 AND file_path = $2
         ORDER  BY chunk_index",
    )
    .bind(repo_path)
    .bind(file_path)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| ChunkRow {
            file_path: r.get("file_path"),
            start_line: r.get("start_line"),
            end_line: r.get("end_line"),
            content: r.get("content"),
            language: r.get("language"),
            symbol_kind: r.get("symbol_kind"),
            rrf_score: None,
        })
        .collect())
}

// ── Documents ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DocRow {
    pub source_path: String,
    pub doc_kind: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub repo_path: String,
    pub rrf_score: f64,
}

pub async fn search_docs_hybrid(
    pool: &PgPool,
    query_text: &str,
    query_vec: &[f32],
    limit: i32,
    doc_kind: Option<&str>,
) -> Result<Vec<DocRow>> {
    let vec_str = vec_literal(query_vec);
    let candidates = (limit * 3).max(20);

    let rows = sqlx::query(
        "WITH bm25_ranked AS (
             SELECT id, paradedb.score(id) AS bm25_score
             FROM documents
             WHERE id @@@ paradedb.match('content', $1)
               AND ($3 IS NULL OR doc_kind = $3)
             LIMIT 60
         ),
         bm25_with_rank AS (
             SELECT id, ROW_NUMBER() OVER (ORDER BY bm25_score DESC) AS rank
             FROM bm25_ranked
         ),
         vec_ranked AS (
             SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $2::vector) AS rank
             FROM documents
             WHERE embedding IS NOT NULL
               AND ($3 IS NULL OR doc_kind = $3)
             ORDER BY embedding <=> $2::vector
             LIMIT 60
         ),
         fused AS (
             SELECT COALESCE(b.id, v.id) AS id,
                    COALESCE(1.0 / (60 + b.rank), 0.0)
                  + COALESCE(1.0 / (60 + v.rank), 0.0) AS rrf_score
             FROM bm25_with_rank b
             FULL OUTER JOIN vec_ranked v USING (id)
         )
         SELECT d.source_path, d.doc_kind, d.title, d.content, d.repo_path, f.rrf_score
         FROM fused f
         JOIN documents d USING (id)
         ORDER BY f.rrf_score DESC
         LIMIT $4",
    )
    .bind(query_text)
    .bind(&vec_str)
    .bind(doc_kind)
    .bind(candidates)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|r| DocRow {
            source_path: r.get("source_path"),
            doc_kind: r.get("doc_kind"),
            title: r.get("title"),
            content: r.get("content"),
            repo_path: r.get("repo_path"),
            rrf_score: r.get::<f64, _>("rrf_score"),
        })
        .collect::<Vec<_>>()
        .into_iter()
        .take(limit as usize)
        .collect())
}

// ── GitHub ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GithubRow {
    pub entity_type: String, // "issue" or "pr"
    pub repo: String,
    pub number: i32,
    pub state: Option<String>,
    pub title: String,
    pub author: Option<String>,
    pub labels: Option<String>,
    pub content: String,
    pub created_at: Option<String>,
    pub rrf_score: f64,
}

/// Hybrid search across github_issues and github_prs, merging and re-ranking by RRF score.
pub async fn search_github_hybrid(
    pool: &PgPool,
    query_text: &str,
    query_vec: &[f32],
    limit: i32,
    repo: Option<&str>,
    state: Option<&str>,
    entity_type: Option<&str>, // "issues", "prs", or None for both
) -> Result<Vec<GithubRow>> {
    let vec_str = vec_literal(query_vec);
    let want_issues = entity_type.map(|t| t == "issues").unwrap_or(true);
    let want_prs = entity_type.map(|t| t == "prs").unwrap_or(true);

    let mut all: Vec<GithubRow> = Vec::new();

    // Search issues
    if want_issues {
        let rows = sqlx::query(
            "WITH bm25_ranked AS (
                 SELECT id, paradedb.score(id) AS bm25_score
                 FROM github_issues
                 WHERE id @@@ paradedb.match('content', $1)
                   AND ($3 IS NULL OR repo = $3)
                   AND ($4 IS NULL OR state = $4)
                 LIMIT 60
             ),
             bm25_with_rank AS (
                 SELECT id, ROW_NUMBER() OVER (ORDER BY bm25_score DESC) AS rank
                 FROM bm25_ranked
             ),
             vec_ranked AS (
                 SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $2::vector) AS rank
                 FROM github_issues
                 WHERE embedding IS NOT NULL
                   AND ($3 IS NULL OR repo = $3)
                   AND ($4 IS NULL OR state = $4)
                 ORDER BY embedding <=> $2::vector
                 LIMIT 60
             ),
             fused AS (
                 SELECT COALESCE(b.id, v.id) AS id,
                        COALESCE(1.0 / (60 + b.rank), 0.0)
                      + COALESCE(1.0 / (60 + v.rank), 0.0) AS rrf_score
                 FROM bm25_with_rank b
                 FULL OUTER JOIN vec_ranked v USING (id)
             )
             SELECT i.repo, i.number, i.state, i.title, i.author, i.labels,
                    i.content, i.created_at::TEXT, f.rrf_score
             FROM fused f
             JOIN github_issues i USING (id)
             ORDER BY f.rrf_score DESC
             LIMIT $5",
        )
        .bind(query_text)
        .bind(&vec_str)
        .bind(repo)
        .bind(state)
        .bind(limit)
        .fetch_all(pool)
        .await?;

        for r in &rows {
            all.push(GithubRow {
                entity_type: "issue".into(),
                repo: r.get("repo"),
                number: r.get("number"),
                state: r.get("state"),
                title: r.get("title"),
                author: r.get("author"),
                labels: r.get("labels"),
                content: r.get("content"),
                created_at: r.get("created_at"),
                rrf_score: r.get::<f64, _>("rrf_score"),
            });
        }
    }

    // Search PRs
    if want_prs {
        let rows = sqlx::query(
            "WITH bm25_ranked AS (
                 SELECT id, paradedb.score(id) AS bm25_score
                 FROM github_prs
                 WHERE id @@@ paradedb.match('content', $1)
                   AND ($3 IS NULL OR repo = $3)
                   AND ($4 IS NULL OR state = $4)
                 LIMIT 60
             ),
             bm25_with_rank AS (
                 SELECT id, ROW_NUMBER() OVER (ORDER BY bm25_score DESC) AS rank
                 FROM bm25_ranked
             ),
             vec_ranked AS (
                 SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> $2::vector) AS rank
                 FROM github_prs
                 WHERE embedding IS NOT NULL
                   AND ($3 IS NULL OR repo = $3)
                   AND ($4 IS NULL OR state = $4)
                 ORDER BY embedding <=> $2::vector
                 LIMIT 60
             ),
             fused AS (
                 SELECT COALESCE(b.id, v.id) AS id,
                        COALESCE(1.0 / (60 + b.rank), 0.0)
                      + COALESCE(1.0 / (60 + v.rank), 0.0) AS rrf_score
                 FROM bm25_with_rank b
                 FULL OUTER JOIN vec_ranked v USING (id)
             )
             SELECT p.repo, p.number, p.state, p.title, p.author, p.labels,
                    p.content, p.created_at::TEXT, f.rrf_score
             FROM fused f
             JOIN github_prs p USING (id)
             ORDER BY f.rrf_score DESC
             LIMIT $5",
        )
        .bind(query_text)
        .bind(&vec_str)
        .bind(repo)
        .bind(state)
        .bind(limit)
        .fetch_all(pool)
        .await?;

        for r in &rows {
            all.push(GithubRow {
                entity_type: "pr".into(),
                repo: r.get("repo"),
                number: r.get("number"),
                state: r.get("state"),
                title: r.get("title"),
                author: r.get("author"),
                labels: r.get("labels"),
                content: r.get("content"),
                created_at: r.get("created_at"),
                rrf_score: r.get::<f64, _>("rrf_score"),
            });
        }
    }

    // Merge issues + PR results by RRF score
    all.sort_by(|a, b| b.rrf_score.partial_cmp(&a.rrf_score).unwrap_or(std::cmp::Ordering::Equal));
    all.truncate(limit as usize);
    Ok(all)
}
