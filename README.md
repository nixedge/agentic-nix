# agentic-nix

Hybrid code search for Claude. Index your codebases, documentation, and GitHub issues into
PostgreSQL (ParadeDB BM25 + pgvector HNSW). A Rust MCP server exposes the index to Claude Code
so it can answer questions about your code without reading every file from scratch.

**What this replaces:** Claude reading 50 files one by one to find something. Instead, Claude
queries the index with a single tool call and gets the 10 most relevant chunks in under a second.

---

## How it works

```
your repos ──► ingest ──► PostgreSQL (ParadeDB)
                               │
                          BM25 + vector          Ollama
                          hybrid search    ◄──  embeddings
                               │
                          MCP server ──► Claude Code
```

1. The `ingest` binary walks your repos, extracts named symbols via tree-sitter, embeds each
   chunk via Ollama, and stores everything in PostgreSQL.
2. The `mcp-server` binary sits between Claude and the database, exposing search tools over
   the MCP protocol (stdio).
3. Claude Code connects to the MCP server automatically and calls the tools when it needs
   to understand code.

---

## Prerequisites

- [Nix](https://nixos.org/download/) with flakes enabled
- Git

That's it. PostgreSQL, Ollama, Rust, and all other tools are managed by Nix.

**Enable flakes** if you haven't already — add to `~/.config/nix/nix.conf`:
```
experimental-features = nix-command flakes
```

---

## First-time setup

### 1. Clone and enter the dev shell

```bash
git clone <this-repo> ~/agentic-nix
cd ~/agentic-nix
nix develop
```

The first `nix develop` downloads Rust, PostgreSQL, Ollama, and all dependencies. This takes
a few minutes once; subsequent shells are instant.

### 2. Build the Rust binaries

```bash
just build
```

This produces `target/release/mcp-server` and `target/release/ingest`. You only need to
rebuild when the Rust source changes.

### 3. Start the services

In a dedicated terminal (keep it running):

```bash
nix run .#dev
```

This starts:
- **PostgreSQL 17** on `localhost:5432` with `pg_search` (BM25) and `pgvector` (HNSW) loaded.
  The `codebase` database is created automatically with the full schema.
- **Ollama** on `localhost:11434` with the `jina-code-embeddings-1.5b` model pulled.
  First start pulls the model (~1.5 GB); subsequent starts are instant.

Data is stored under `./data/` relative to where you run the command, so always run it from
the repo root.

### 4. Verify everything is running

```bash
# Should return a connection
just psql

# Should return empty tables (schema applied automatically)
just stats
```

---

## Indexing your code

### Index a codebase

```bash
just index /path/to/your/repo
```

This walks the repo, extracts symbols for TypeScript, JavaScript, Python, Rust, and Haskell
files using tree-sitter (functions, classes, structs, etc.), and falls back to overlapping
line windows for other file types. Files that haven't changed since the last run are skipped.

Re-index everything from scratch:
```bash
just reindex /path/to/your/repo
```

Index multiple repos — just run the command once per repo:
```bash
just index ~/work/frontend
just index ~/work/backend
just index ~/work/infrastructure
```

### Index documentation

Discovers `AGENTS.md`, `CLAUDE.md`, `README.md`, and any `.agent/workflows/`, `.agent/skills/`,
`.agent/plans/`, `.agent/SOPs/` markdown files:

```bash
just index-docs /path/to/your/repo
```

### Index GitHub issues and pull requests

```bash
export GITHUB_TOKEN=ghp_...   # optional but recommended (5000 req/hr vs 60)
just index-github anthropics/claude-code
```

This fetches all issues, PRs, and their comments. Subsequent runs are incremental — only items
updated since the last sync are fetched.

Index a specific stream only:
```bash
just index-github-issues anthropics/claude-code   # issues only
just index-github-prs    anthropics/claude-code   # PRs only
```

### Check what's indexed

```bash
just stats          # row counts per table
just sync-status    # GitHub watermarks (last sync times)
```

---

## Connect to Claude Code

The MCP server communicates with Claude Code over stdio. You need to register it once.

### Add the MCP server

Run this from the `~/agentic-nix` directory:

```bash
claude mcp add agentic-nix \
  --command "$(pwd)/target/release/mcp-server" \
  --env PG_DSN=postgresql://127.0.0.1:5432/codebase \
  --env OLLAMA_HOST=http://127.0.0.1:11434
```

Or add it manually to your Claude Code config (`~/.claude.json` or project `.claude/mcp.json`):

```json
{
  "mcpServers": {
    "agentic-nix": {
      "command": "/home/you/agentic-nix/target/release/mcp-server",
      "env": {
        "PG_DSN": "postgresql://127.0.0.1:5432/codebase",
        "OLLAMA_HOST": "http://127.0.0.1:11434"
      }
    }
  }
}
```

### Verify the connection

Start a new Claude Code session and ask:

```
List all indexed repositories.
```

Claude should call `list_repos` and return the repos you've indexed. If it can't connect,
check that the services are running (`nix run .#dev`) and the binary path is correct.

---

## Using the index with Claude

### How Claude uses the tools automatically

Once connected, Claude will automatically call the search tools when it needs to understand
your code. You don't have to do anything special — just work normally. For example:

- *"How does the authentication middleware work?"* → Claude searches for auth-related code
- *"Why was this API endpoint added?"* → Claude searches GitHub issues for context
- *"What does the `UserService` class do?"* → Claude fetches chunks for that symbol

### Prompts that get the most out of the index

Being explicit helps Claude know to search rather than guess:

```
Search the codebase for how we handle database connection pooling.

Look through the indexed GitHub issues for any discussion of rate limiting.

Find all the Rust functions related to embedding and explain how they fit together.

Search the docs for our deployment workflow.
```

### Available tools

| Tool | When to use it |
|---|---|
| `search_code` | Natural language or code questions about the codebase |
| `bm25_search` | Exact identifier or symbol lookups (faster, no embeddings) |
| `search_docs` | Questions about workflows, SOPs, or agent instructions |
| `search_github` | "Was there an issue about X?" or "How was Y implemented?" |
| `list_repos` | See what's indexed |
| `get_file` | Read a complete file by path |

You can guide Claude toward a specific tool:

```
Use bm25_search to find every place we call `send_email`.

Search GitHub PRs for anything related to the login refactor.

Search only Haskell files for the `parseConfig` function.
```

### Filters

The search tools accept optional filters you can mention in your prompt:

- **Language**: `search in Rust files`, `TypeScript only`
- **Symbol kind**: `only functions`, `find all classes`, `interfaces only`
- **Doc kind**: `search workflows`, `look in SOPs`
- **GitHub**: `open issues only`, `only PRs`, `in the anthropics/claude-code repo`

---

## Keeping the index current

### After pulling new code

Re-index is incremental — only changed files are processed:

```bash
just index /path/to/your/repo
```

### Scheduled re-indexing (optional)

Add to your crontab or a systemd timer:

```bash
# Re-index at 2am every night
0 2 * * * cd ~/agentic-nix && nix develop --command just index ~/work/myrepo
```

### After large refactors

If you've moved many files around, a full re-index is cleaner:

```bash
just reindex /path/to/your/repo
```

---

## Environment variables

All variables have sensible defaults; override as needed.

| Variable | Default | Description |
|---|---|---|
| `PG_DSN` | `postgresql://127.0.0.1:5432/codebase` | PostgreSQL connection string |
| `OLLAMA_HOST` | `http://127.0.0.1:11434` | Ollama API base URL |
| `EMBED_MODEL` | `hf.co/jinaai/jina-code-embeddings-1.5b-GGUF:Q8_0` | Embedding model |
| `RERANK_MODEL` | *(empty — disabled)* | Set to a fastembed cross-encoder to enable reranking |
| `GITHUB_TOKEN` | *(empty)* | GitHub personal access token (raises rate limit to 5000/hr) |

---

## Quick reference

```bash
# Services
nix run .#dev                          # start PostgreSQL + Ollama

# Binaries
just build                             # build mcp-server + ingest

# Indexing
just index /path/to/repo               # code (incremental)
just reindex /path/to/repo             # code (force full re-index)
just index-docs /path/to/repo          # markdown docs
just index-github OWNER/REPO           # GitHub issues + PRs
just reindex-github OWNER/REPO         # GitHub (ignore watermarks)

# Inspection
just stats                             # row counts
just sync-status                       # GitHub watermarks
just psql                              # open a psql session

# Schema
just migrate                           # apply schema.sql (safe to re-run)
```

---

## Troubleshooting

**"Embedding failed (is Ollama running?)"**
The MCP server can't reach Ollama. Make sure `nix run .#dev` is running, or check that
`OLLAMA_HOST` points to the right address.

**"Database error: connection refused"**
PostgreSQL isn't running. Start it with `nix run .#dev`. Data lives in `./data/pg/` —
you must run from the repo root so the path resolves correctly.

**Claude doesn't call the search tools**
Claude doesn't always reach for MCP tools unprompted. Be explicit: *"Search the indexed
codebase for..."* or *"Use the search tools to find..."*.

**Slow first query after starting**
The embedding model loads lazily on the first request. Subsequent queries are fast.

**GitHub rate limit hit**
Set `GITHUB_TOKEN` with a personal access token. The unauth limit is 60 requests/hour;
authenticated is 5000/hour.
