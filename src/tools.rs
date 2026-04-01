use rmcp::{
    Error as McpError,
    ServerHandler,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars,
    tool,
};
use serde::Deserialize;
use sqlx::PgPool;

use crate::{db, embed, fmt, rerank};

const RERANK_POOL_SIZE: i32 = 20;

fn default_limit() -> i32 {
    10
}

// ── Parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchCodeParams {
    #[schemars(description = "Natural language or code search query")]
    pub query: String,
    #[schemars(description = "Number of results to return (default 10)")]
    #[serde(default = "default_limit")]
    pub limit: i32,
    #[schemars(description = "Filter by language: typescript, python, rust, go, etc.")]
    #[serde(default)]
    pub language: Option<String>,
    #[schemars(description = "Filter by symbol kind: function, class, interface, type, enum, chunk")]
    #[serde(default)]
    pub symbol_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchDocsParams {
    #[schemars(description = "Natural language search query")]
    pub query: String,
    #[schemars(description = "Number of results to return (default 10)")]
    #[serde(default = "default_limit")]
    pub limit: i32,
    #[schemars(description = "Filter by document kind: readme, workflow, sop, plan, skill, agent_instruction")]
    #[serde(default)]
    pub doc_kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchGithubParams {
    #[schemars(description = "Natural language search query")]
    pub query: String,
    #[schemars(description = "Number of results to return (default 10)")]
    #[serde(default = "default_limit")]
    pub limit: i32,
    #[schemars(description = "Filter by entity type: issues or prs (default: both)")]
    #[serde(default)]
    pub entity_type: Option<String>,
    #[schemars(description = "Filter by repo: OWNER/REPO")]
    #[serde(default)]
    pub repo: Option<String>,
    #[schemars(description = "Filter by state: open, closed, merged")]
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetFileParams {
    #[schemars(description = "Repository root path (from list_repos)")]
    pub repo_path: String,
    #[schemars(description = "File path relative to the repo root")]
    pub file_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FetchPackageParams {
    #[schemars(description = "Haskell package name (e.g. 'serialise', 'cardano-ledger-core')")]
    pub package: String,
    #[schemars(description = "Package version (e.g. '0.2.6.1')")]
    pub version: String,
}

// ── Server ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CodeSearchServer {
    pool: PgPool,
}

#[tool(tool_box)]
impl CodeSearchServer {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // ── search_code ───────────────────────────────────────────────────────────

    #[tool(
        description = "Hybrid BM25+vector search over indexed code with optional cross-encoder reranking. \
                       Supports filtering by language and symbol_kind (function, class, etc.)."
    )]
    async fn search_code(
        &self,
        #[tool(aggr)] params: SearchCodeParams,
    ) -> Result<CallToolResult, McpError> {
        let SearchCodeParams { query, limit, language, symbol_kind } = params;

        let query_vec = match embed::embed(&query).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Embedding failed (is Ollama running?): {e}"
                ))]));
            }
        };

        let rerank_enabled = std::env::var("RERANK_MODEL")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let candidates = if rerank_enabled {
            limit.max(RERANK_POOL_SIZE)
        } else {
            limit
        };

        let lang_ref = language.as_deref();
        let kind_ref = symbol_kind.as_deref();

        let rows = match db::hybrid_search(
            &self.pool, &query, &query_vec, candidates, lang_ref, kind_ref,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Database error: {e}"
                ))]));
            }
        };

        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No results found.",
            )]));
        }

        let docs: Vec<String> = rows.iter().map(|r| r.content.clone()).collect();
        let scores = rerank::rerank(&query, &docs).await;

        let text = if let Some(ref score_vec) = scores {
            let mut indexed: Vec<(usize, f32)> =
                score_vec.iter().copied().enumerate().collect();
            indexed
                .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            let top: Vec<_> = indexed.into_iter().take(limit as usize).collect();
            let final_rows: Vec<db::ChunkRow> =
                top.iter().map(|(i, _)| rows[*i].clone()).collect();
            let final_scores: Vec<f32> = top.iter().map(|(_, s)| *s).collect();
            fmt::fmt_chunks(&final_rows, Some(&final_scores), "rerank")
        } else {
            fmt::fmt_chunks(&rows[..rows.len().min(limit as usize)], None, "rrf")
        };

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    // ── bm25_search ───────────────────────────────────────────────────────────

    #[tool(
        description = "BM25 full-text only search over code. Best for exact identifiers, symbols, or keywords. \
                       Supports filtering by language and symbol_kind."
    )]
    async fn bm25_search(
        &self,
        #[tool(aggr)] params: SearchCodeParams,
    ) -> Result<CallToolResult, McpError> {
        let SearchCodeParams { query, limit, language, symbol_kind } = params;

        let rows = match db::bm25_search(
            &self.pool,
            &query,
            limit,
            language.as_deref(),
            symbol_kind.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Database error: {e}"
                ))]));
            }
        };
        Ok(CallToolResult::success(vec![Content::text(
            fmt::fmt_chunks(&rows, None, "bm25"),
        )]))
    }

    // ── search_docs ───────────────────────────────────────────────────────────

    #[tool(
        description = "Hybrid BM25+vector search over indexed documentation: README files, \
                       AGENTS.md, CLAUDE.md, and .agent/** workflow/skill/plan/SOP files. \
                       Filter by doc_kind: readme, workflow, sop, plan, skill, agent_instruction."
    )]
    async fn search_docs(
        &self,
        #[tool(aggr)] params: SearchDocsParams,
    ) -> Result<CallToolResult, McpError> {
        let SearchDocsParams { query, limit, doc_kind } = params;

        let query_vec = match embed::embed(&query).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Embedding failed (is Ollama running?): {e}"
                ))]));
            }
        };

        let rows = match db::search_docs_hybrid(
            &self.pool,
            &query,
            &query_vec,
            limit,
            doc_kind.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Database error: {e}"
                ))]));
            }
        };

        Ok(CallToolResult::success(vec![Content::text(fmt::fmt_docs(
            &rows,
        ))]))
    }

    // ── search_github ─────────────────────────────────────────────────────────

    #[tool(
        description = "Hybrid BM25+vector search over indexed GitHub issues and pull requests. \
                       Filter by entity_type (issues/prs), repo (OWNER/REPO), or state (open/closed/merged)."
    )]
    async fn search_github(
        &self,
        #[tool(aggr)] params: SearchGithubParams,
    ) -> Result<CallToolResult, McpError> {
        let SearchGithubParams { query, limit, entity_type, repo, state } = params;

        let query_vec = match embed::embed(&query).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Embedding failed (is Ollama running?): {e}"
                ))]));
            }
        };

        let rows = match db::search_github_hybrid(
            &self.pool,
            &query,
            &query_vec,
            limit,
            repo.as_deref(),
            state.as_deref(),
            entity_type.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Database error: {e}"
                ))]));
            }
        };

        Ok(CallToolResult::success(vec![Content::text(fmt::fmt_github(
            &rows,
        ))]))
    }

    // ── list_repos ────────────────────────────────────────────────────────────

    #[tool(description = "List all indexed repositories with chunk and file counts.")]
    async fn list_repos(&self) -> Result<CallToolResult, McpError> {
        let repos = match db::list_repos(&self.pool).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Database error: {e}"
                ))]));
            }
        };
        if repos.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No repositories indexed yet.\nRun: just index /path/to/repo",
            )]));
        }
        let mut lines = vec!["Indexed repositories:\n".to_string()];
        for r in &repos {
            lines.push(format!(
                "  {}\n    {} files · {} chunks · last indexed {}",
                r.repo_path, r.files, r.chunks, r.last_indexed
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    // ── fetch_package ─────────────────────────────────────────────────────────

    #[tool(
        description = "Fetch a Haskell package from CHaP (Cardano Haskell Package repository) or \
                       Hackage and index it for code search. Checks CHaP first, falls back to \
                       Hackage. Returns immediately if the package is already indexed. Use this \
                       when you need source-level detail about a Haskell library (types, functions, \
                       instances) that isn't in the current index."
    )]
    async fn fetch_package(
        &self,
        #[tool(aggr)] params: FetchPackageParams,
    ) -> Result<CallToolResult, McpError> {
        let FetchPackageParams { package, version } = params;
        let pkg_ver = format!("{package}-{version}");
        let chap_key = format!("chap::{pkg_ver}");
        let hackage_key = format!("hackage::{pkg_ver}");

        // Check if already indexed.
        let existing: i64 = match sqlx::query_scalar(
            "SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1 OR repo_path = $2",
        )
        .bind(&chap_key)
        .bind(&hackage_key)
        .fetch_one(&self.pool)
        .await
        {
            Ok(n) => n,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Database error: {e}"
                ))]));
            }
        };

        if existing > 0 {
            let source: String = sqlx::query_scalar(
                "SELECT repo_path FROM code_chunks WHERE repo_path = $1 OR repo_path = $2 LIMIT 1",
            )
            .bind(&chap_key)
            .bind(&hackage_key)
            .fetch_one(&self.pool)
            .await
            .unwrap_or_else(|_| pkg_ver.clone());

            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Already indexed: {pkg_ver} ({existing} chunks, source: {source}).\n\
                 Use search_code with language=haskell to query it."
            ))]));
        }

        // Shell out to the ingest binary.
        let ingest_bin = find_ingest_bin();
        let pg_dsn = std::env::var("PG_DSN")
            .unwrap_or_else(|_| "postgresql://127.0.0.1:5432/codebase".into());

        let output = match tokio::process::Command::new(&ingest_bin)
            .args(["hackage", &package, &version])
            .env("PG_DSN", &pg_dsn)
            .output()
            .await
        {
            Ok(o) => o,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Failed to launch ingest binary ({ingest_bin:?}): {e}"
                ))]));
            }
        };

        let stderr_log = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Ingest failed (exit {}):\n{stderr_log}",
                output.status.code().unwrap_or(-1)
            ))]));
        }

        // Query final stats.
        let (chunks, source) = {
            let chap_count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1",
            )
            .bind(&chap_key)
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);

            if chap_count > 0 {
                (chap_count, "chap".to_string())
            } else {
                let hackage_count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM code_chunks WHERE repo_path = $1",
                )
                .bind(&hackage_key)
                .fetch_one(&self.pool)
                .await
                .unwrap_or(0);
                (hackage_count, "hackage".to_string())
            }
        };

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Indexed {pkg_ver} from {source}: {chunks} chunks.\n\
             You can now use search_code with language=haskell to query this package.\n\
             Log:\n{stderr_log}"
        ))]))
    }

    // ── get_file ──────────────────────────────────────────────────────────────

    #[tool(description = "Retrieve all indexed chunks for a specific file in order.")]
    async fn get_file(
        &self,
        #[tool(aggr)] params: GetFileParams,
    ) -> Result<CallToolResult, McpError> {
        let GetFileParams { repo_path, file_path } = params;

        let rows = match db::get_file_chunks(&self.pool, &repo_path, &file_path).await {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Database error: {e}"
                ))]));
            }
        };
        if rows.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "No chunks found for {repo_path}/{file_path}"
            ))]));
        }
        let mut parts = vec![format!("### {file_path}")];
        for r in &rows {
            let lang = r.language.as_deref().unwrap_or("");
            let kind_tag = r
                .symbol_kind
                .as_deref()
                .map(|k| format!(" [{k}]"))
                .unwrap_or_default();
            parts.push(format!(
                "Lines {}-{}{}:\n```{lang}\n{}\n```",
                r.start_line.unwrap_or(0),
                r.end_line.unwrap_or(0),
                kind_tag,
                r.content
            ));
        }
        Ok(CallToolResult::success(vec![Content::text(
            parts.join("\n\n"),
        )]))
    }
}

#[tool(tool_box)]
impl ServerHandler for CodeSearchServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(
                "Hybrid BM25+vector code search with optional cross-encoder reranking.\n\
                 \n\
                 Tools:\n\
                 - search_code: hybrid search over indexed code (language, symbol_kind filters)\n\
                 - bm25_search: BM25-only, best for exact identifiers and symbols\n\
                 - search_docs: hybrid search over indexed markdown docs (doc_kind filter)\n\
                 - search_github: hybrid search over indexed GitHub issues + PRs (entity_type, repo, state filters)\n\
                 - list_repos: list indexed repositories with file and chunk counts\n\
                 - get_file: retrieve all chunks for a specific file\n\
                 - fetch_package: download and index a Haskell package from CHaP or Hackage on demand\n\
                 \n\
                 IMPORTANT — Haskell packages:\n\
                 When you need source-level detail about a Haskell library (types, functions, instances, \
                 module structure), call fetch_package FIRST before attempting to read files from \
                 /nix/store or searching the filesystem. fetch_package indexes the package into the \
                 search database so subsequent search_code calls can find it. It is a no-op if the \
                 package is already indexed. Example: to understand the serialise package, call \
                 fetch_package({\"package\": \"serialise\", \"version\": \"0.2.6.1\"}) then \
                 search_code({\"query\": \"...\", \"language\": \"haskell\"})."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Locate the `ingest` binary: prefer the sibling in the same directory as this
/// process, then fall back to whatever is on PATH.
fn find_ingest_bin() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("ingest");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    std::path::PathBuf::from("ingest")
}
