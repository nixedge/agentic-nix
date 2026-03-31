-- Agentic RAG schema
-- Safe to run on an existing database: all statements use IF NOT EXISTS / IF NOT EXISTS guards.

CREATE EXTENSION IF NOT EXISTS vector;

-- ── Code chunks ───────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS code_chunks (
    id           BIGSERIAL PRIMARY KEY,
    repo_path    TEXT        NOT NULL,
    file_path    TEXT        NOT NULL,
    chunk_index  INTEGER     NOT NULL,
    content      TEXT        NOT NULL,
    start_line   INTEGER,
    end_line     INTEGER,
    language     TEXT,
    symbol_kind  TEXT,
    content_hash TEXT,
    file_mtime   BIGINT,
    embedding    VECTOR(1536),
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (repo_path, file_path, chunk_index)
);

CREATE INDEX IF NOT EXISTS code_chunks_bm25
    ON code_chunks
    USING bm25 (id, content, language, file_path)
    WITH (key_field = 'id');

CREATE INDEX IF NOT EXISTS code_chunks_hnsw
    ON code_chunks
    USING hnsw (embedding vector_cosine_ops);

-- ── Documents (markdown) ──────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS documents (
    id           BIGSERIAL PRIMARY KEY,
    repo_path    TEXT        NOT NULL,
    source_path  TEXT        NOT NULL,
    chunk_index  INTEGER     NOT NULL,
    doc_kind     TEXT,
    title        TEXT,
    content      TEXT        NOT NULL,
    preview      TEXT,
    content_hash TEXT,
    file_mtime   BIGINT,
    embedding    VECTOR(1536),
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (repo_path, source_path, chunk_index)
);

CREATE INDEX IF NOT EXISTS documents_bm25
    ON documents
    USING bm25 (id, content, doc_kind, source_path)
    WITH (key_field = 'id');

CREATE INDEX IF NOT EXISTS documents_hnsw
    ON documents
    USING hnsw (embedding vector_cosine_ops);

-- ── GitHub issues ─────────────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS github_issues (
    id           BIGSERIAL PRIMARY KEY,
    repo         TEXT        NOT NULL,
    number       INTEGER     NOT NULL,
    state        TEXT,
    title        TEXT        NOT NULL,
    body         TEXT,
    author       TEXT,
    labels       TEXT,
    content      TEXT        NOT NULL,
    created_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ,
    closed_at    TIMESTAMPTZ,
    content_hash TEXT,
    embedding    VECTOR(1536),
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (repo, number)
);

CREATE INDEX IF NOT EXISTS github_issues_bm25
    ON github_issues
    USING bm25 (id, content, repo, state)
    WITH (key_field = 'id');

CREATE INDEX IF NOT EXISTS github_issues_hnsw
    ON github_issues
    USING hnsw (embedding vector_cosine_ops);

-- ── GitHub issue comments ─────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS github_issue_comments (
    id           BIGSERIAL PRIMARY KEY,
    repo         TEXT        NOT NULL,
    issue_number INTEGER     NOT NULL,
    comment_id   BIGINT      NOT NULL,
    author       TEXT,
    content      TEXT        NOT NULL,
    created_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ,
    content_hash TEXT,
    embedding    VECTOR(1536),
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (repo, comment_id)
);

CREATE INDEX IF NOT EXISTS github_issue_comments_hnsw
    ON github_issue_comments
    USING hnsw (embedding vector_cosine_ops);

-- ── GitHub pull requests ──────────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS github_prs (
    id           BIGSERIAL PRIMARY KEY,
    repo         TEXT        NOT NULL,
    number       INTEGER     NOT NULL,
    state        TEXT,
    title        TEXT        NOT NULL,
    body         TEXT,
    author       TEXT,
    labels       TEXT,
    base_branch  TEXT,
    head_branch  TEXT,
    content      TEXT        NOT NULL,
    created_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ,
    merged_at    TIMESTAMPTZ,
    content_hash TEXT,
    embedding    VECTOR(1536),
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (repo, number)
);

CREATE INDEX IF NOT EXISTS github_prs_bm25
    ON github_prs
    USING bm25 (id, content, repo, state)
    WITH (key_field = 'id');

CREATE INDEX IF NOT EXISTS github_prs_hnsw
    ON github_prs
    USING hnsw (embedding vector_cosine_ops);

-- ── GitHub PR review comments ─────────────────────────────────────────────────

CREATE TABLE IF NOT EXISTS github_pr_comments (
    id           BIGSERIAL PRIMARY KEY,
    repo         TEXT        NOT NULL,
    pr_number    INTEGER     NOT NULL,
    comment_id   BIGINT      NOT NULL,
    comment_type TEXT        NOT NULL DEFAULT 'review',
    author       TEXT,
    content      TEXT        NOT NULL,
    file_path    TEXT,
    diff_hunk    TEXT,
    created_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ,
    content_hash TEXT,
    embedding    VECTOR(1536),
    indexed_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (repo, comment_type, comment_id)
);

CREATE INDEX IF NOT EXISTS github_pr_comments_hnsw
    ON github_pr_comments
    USING hnsw (embedding vector_cosine_ops);

-- ── Sync state (watermarks for incremental GitHub sync) ───────────────────────

CREATE TABLE IF NOT EXISTS sync_state (
    id            TEXT        PRIMARY KEY,
    source_name   TEXT        NOT NULL,
    scope_key     TEXT        NOT NULL,
    watermark     TEXT,
    last_synced_at TIMESTAMPTZ,
    error_count   INTEGER     NOT NULL DEFAULT 0,
    last_error    TEXT,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (source_name, scope_key)
);

-- ── Hybrid search function (BM25 + vector RRF) ────────────────────────────────

CREATE OR REPLACE FUNCTION hybrid_search(
    query_text  TEXT,
    query_vec   VECTOR,
    match_count INTEGER
)
RETURNS TABLE (
    id        BIGINT,
    file_path TEXT,
    start_line INTEGER,
    end_line   INTEGER,
    content    TEXT,
    language   TEXT,
    rrf_score  FLOAT8
)
LANGUAGE SQL
AS $$
    WITH bm25_ranked AS (
        SELECT id, paradedb.score(id) AS bm25_score
        FROM code_chunks
        WHERE id @@@ paradedb.match('content', query_text)
        LIMIT 60
    ),
    bm25_with_rank AS (
        SELECT id, ROW_NUMBER() OVER (ORDER BY bm25_score DESC) AS rank
        FROM bm25_ranked
    ),
    vec_ranked AS (
        SELECT id, ROW_NUMBER() OVER (ORDER BY embedding <=> query_vec) AS rank
        FROM code_chunks
        WHERE embedding IS NOT NULL
        ORDER BY embedding <=> query_vec
        LIMIT 60
    ),
    fused AS (
        SELECT COALESCE(b.id, v.id) AS id,
               COALESCE(1.0 / (60 + b.rank), 0.0)
             + COALESCE(1.0 / (60 + v.rank), 0.0) AS rrf_score
        FROM bm25_with_rank b
        FULL OUTER JOIN vec_ranked v USING (id)
    )
    SELECT c.id, c.file_path, c.start_line, c.end_line, c.content, c.language,
           f.rrf_score
    FROM fused f
    JOIN code_chunks c USING (id)
    ORDER BY f.rrf_score DESC
    LIMIT match_count;
$$;
