use rmcp::{
    Error as McpError,
    ServerHandler,
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars,
    tool,
};
use serde::Deserialize;
use sqlx::PgPool;

use mcp_server::ingest::{crates::ingest_crate, hackage::ingest_hackage};

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
    #[schemars(description = "Package or crate name (e.g. 'serialise', 'tokio')")]
    pub package: String,
    #[schemars(description = "Package version (e.g. '0.2.6.1', '1.0.0')")]
    pub version: String,
    #[schemars(
        description = "Package ecosystem: 'haskell' (CHaP/Hackage, default) or 'rust' (crates.io)"
    )]
    #[serde(default = "default_ecosystem")]
    pub ecosystem: String,
}

fn default_ecosystem() -> String {
    "haskell".to_string()
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
        description = "Fetch a package from crates.io (Rust) or CHaP/Hackage (Haskell) and index \
                       it for code search. Returns immediately if already indexed. Use this when \
                       you need source-level detail about a library that isn't in the current index. \
                       Set ecosystem='rust' for Rust crates, ecosystem='haskell' (default) for \
                       Haskell packages (checks CHaP first, falls back to Hackage)."
    )]
    async fn fetch_package(
        &self,
        #[tool(aggr)] params: FetchPackageParams,
    ) -> Result<CallToolResult, McpError> {
        let FetchPackageParams { package, version, ecosystem } = params;
        let pkg_ver = format!("{package}-{version}");

        let (subcommand, repo_path_keys): (&str, Vec<String>) = match ecosystem.as_str() {
            "rust" => (
                "crate",
                vec![format!("crates.io::{pkg_ver}")],
            ),
            _ => (
                "hackage",
                vec![
                    format!("chap::{pkg_ver}"),
                    format!("hackage::{pkg_ver}"),
                ],
            ),
        };

        // Check if already indexed.
        let existing: i64 = {
            let mut q = "SELECT COUNT(*) FROM code_chunks WHERE ".to_string();
            let placeholders: Vec<String> =
                (1..=repo_path_keys.len()).map(|i| format!("repo_path = ${i}")).collect();
            q.push_str(&placeholders.join(" OR "));
            let mut query = sqlx::query_scalar(&q);
            for key in &repo_path_keys {
                query = query.bind(key);
            }
            match query.fetch_one(&self.pool).await {
                Ok(n) => n,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Database error: {e}"
                    ))]));
                }
            }
        };

        if existing > 0 {
            let language = if ecosystem == "rust" { "rust" } else { "haskell" };
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Already indexed: {pkg_ver} ({existing} chunks).\n\
                 Use search_code with language={language} to query it."
            ))]));
        }

        // Call the ingest functions directly (no subprocess).
        let result = match subcommand {
            "hackage" => ingest_hackage(&self.pool, &package, &version, false).await,
            "crate"   => ingest_crate(&self.pool, &package, &version, false).await,
            _         => Err(anyhow::anyhow!("unknown ecosystem: {ecosystem}")),
        };

        if let Err(e) = result {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Ingest failed: {e}"
            ))]));
        }

        // Query final chunk count.
        let chunks: i64 = {
            let mut q = "SELECT COUNT(*) FROM code_chunks WHERE ".to_string();
            let placeholders: Vec<String> =
                (1..=repo_path_keys.len()).map(|i| format!("repo_path = ${i}")).collect();
            q.push_str(&placeholders.join(" OR "));
            let mut query = sqlx::query_scalar(&q);
            for key in &repo_path_keys {
                query = query.bind(key);
            }
            query.fetch_one(&self.pool).await.unwrap_or(0)
        };

        let language = if ecosystem == "rust" { "rust" } else { "haskell" };
        Ok(CallToolResult::success(vec![Content::text(format!(
            "Indexed {pkg_ver} from {ecosystem}: {chunks} chunks.\n\
             Use search_code with language={language} to query it."
        ))]))
    }

    // ── get_file ──────────────────────────────────────────────────────────────

    #[tool(
        description = "Retrieve all indexed chunks for a specific file. Writes the reconstructed \
                       source to /tmp/agentic-nix/ and returns the path — use the Read tool to \
                       inspect it without flooding context."
    )]
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

        // Reconstruct the file from ordered chunks and write to /tmp.
        // Chunks may overlap (overlapping-window fallback), so de-duplicate by
        // collecting all lines into a BTreeMap keyed by 1-indexed line number.
        let mut line_map: std::collections::BTreeMap<i32, String> = std::collections::BTreeMap::new();
        for r in &rows {
            let start = r.start_line.unwrap_or(1).max(1);
            for (offset, line) in r.content.lines().enumerate() {
                line_map.entry(start + offset as i32).or_insert_with(|| line.to_string());
            }
        }
        let source = line_map.values().cloned().collect::<Vec<_>>().join("\n");

        // Build a stable, human-readable path under /tmp/agentic-nix/.
        // Sanitise repo_path (colons → underscores) so it's a valid directory name.
        let safe_repo = repo_path.replace("::", "__").replace(':', "_").replace('/', "_");
        let out_path = std::path::PathBuf::from("/tmp/agentic-nix")
            .join(&safe_repo)
            .join(&file_path);

        if let Some(parent) = out_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Failed to create temp directory: {e}"
                ))]));
            }
        }
        if let Err(e) = std::fs::write(&out_path, &source) {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Failed to write temp file: {e}"
            ))]));
        }

        let lang = rows.first().and_then(|r| r.language.as_deref()).unwrap_or("text");
        Ok(CallToolResult::success(vec![Content::text(format!(
            "File written to: {}\n({} lines, language: {lang})\n\
             Use the Read tool to inspect it.",
            out_path.display(),
            line_map.len(),
        ))]))
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
                 IMPORTANT — external packages:\n\
                 When you need source-level detail about a Haskell or Rust library (types, functions, \
                 instances, module structure), call fetch_package FIRST before attempting to read \
                 files from /nix/store or searching the filesystem. fetch_package downloads and \
                 indexes the package so subsequent search_code calls can find it. It is a no-op if \
                 already indexed.\n\
                 - Haskell: fetch_package({\"package\": \"serialise\", \"version\": \"0.2.6.1\", \"ecosystem\": \"haskell\"})\n\
                 - Rust: fetch_package({\"package\": \"tokio\", \"version\": \"1.0.0\", \"ecosystem\": \"rust\"})\n\
                 Then use search_code with language=haskell or language=rust to query the indexed source."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

