# Agentic RAG stack

# List available commands
default:
    @just --list

# Start PostgreSQL (ParadeDB) + Ollama
dev:
    nix run .#dev

# Apply schema to a running database (safe to run on existing DBs)
migrate:
    psql postgres://127.0.0.1:5432/codebase -f scripts/schema.sql

# ── Code indexing ─────────────────────────────────────────────────────────────

# Index a codebase (usage: just index /path/to/repo)
# Incremental: unchanged files are skipped automatically.
index path:
    cargo run --release --bin ingest -- code "{{path}}"

# Re-index, clearing existing chunks first
reindex path:
    cargo run --release --bin ingest -- code "{{path}}" --force

# ── Doc indexing ──────────────────────────────────────────────────────────────

# Index markdown documentation in a repo (AGENTS.md, README, .agent/** etc.)
# Incremental: unchanged files are skipped automatically.
index-docs path:
    cargo run --release --bin ingest -- docs "{{path}}"

# Re-index docs, clearing existing records first
reindex-docs path:
    cargo run --release --bin ingest -- docs "{{path}}" --force

# ── GitHub indexing ───────────────────────────────────────────────────────────

# Index GitHub issues + PRs for a repo (usage: just index-github OWNER/REPO)
# Incremental: only fetches items updated since the last sync watermark.
# Set GITHUB_TOKEN env var for authenticated requests (5000 req/hr).
index-github repo:
    cargo run --release --bin ingest -- github "{{repo}}"

# Re-fetch all GitHub items, ignoring watermarks
reindex-github repo:
    cargo run --release --bin ingest -- github "{{repo}}" --force

# Index only issues (skips PRs and comments)
index-github-issues repo:
    cargo run --release --bin ingest -- github "{{repo}}" --stream issues

# Index only PRs
index-github-prs repo:
    cargo run --release --bin ingest -- github "{{repo}}" --stream prs

# ── MCP server ────────────────────────────────────────────────────────────────

# Build both binaries
build:
    cargo build --release

# Start the MCP server (Claude Code launches this automatically)
mcp:
    cargo run --release --bin mcp-server

# ── Database ──────────────────────────────────────────────────────────────────

# Connect to the database
psql:
    psql postgres://127.0.0.1:5432/codebase

# Show indexed content counts across all tables
stats:
    psql postgres://127.0.0.1:5432/codebase -c \
      "SELECT 'code_chunks'         AS entity, COUNT(*) FROM code_chunks \
       UNION ALL \
       SELECT 'documents',                     COUNT(*) FROM documents \
       UNION ALL \
       SELECT 'github_issues',                 COUNT(*) FROM github_issues \
       UNION ALL \
       SELECT 'github_prs',                    COUNT(*) FROM github_prs \
       UNION ALL \
       SELECT 'github_issue_comments',         COUNT(*) FROM github_issue_comments \
       UNION ALL \
       SELECT 'github_pr_comments',            COUNT(*) FROM github_pr_comments \
       ORDER BY 1;"

# Show sync state watermarks
sync-status:
    psql postgres://127.0.0.1:5432/codebase -c \
      "SELECT source_name, scope_key, last_synced_at, watermark, error_count, last_error \
       FROM sync_state ORDER BY updated_at DESC;"
