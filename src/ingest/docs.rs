use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;

use crate::embed::{embed_batch, vec_literal};

const EMBED_BATCH: usize = 8;
const MAX_CHUNK_CHARS: usize = 4_000;
const MIN_CHUNK_CHARS: usize = 50;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    ".direnv",
    "result",
];

pub async fn ingest_docs(pool: &PgPool, repo_path: &Path, force: bool) -> Result<()> {
    let repo_str = repo_path
        .canonicalize()?
        .to_string_lossy()
        .into_owned();

    // Discover markdown files matching doc patterns
    let candidates = collect_docs(repo_path);
    if candidates.is_empty() {
        eprintln!("No documentation files found.");
        return Ok(());
    }
    eprintln!("Found {} documentation files in {}", candidates.len(), repo_str);

    if force {
        let n: i64 = sqlx::query_scalar(
            "WITH d AS (DELETE FROM documents WHERE repo_path = $1 RETURNING 1)
             SELECT COUNT(*) FROM d",
        )
        .bind(&repo_str)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        eprintln!("Cleared {} existing document chunks.", n);
    }

    // Load existing (source_path → (mtime_nanos, content_hash)) for incremental skip.
    struct FileInfo { mtime_nanos: Option<i64>, content_hash: String }
    let existing: HashMap<String, FileInfo> = if force {
        HashMap::new()
    } else {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (source_path) source_path, file_mtime, content_hash
             FROM documents
             WHERE repo_path = $1 AND content_hash IS NOT NULL
             ORDER BY source_path, chunk_index",
        )
        .bind(&repo_str)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.iter()
            .map(|r| {
                use sqlx::Row as _;
                let sp: String = r.get("source_path");
                let mt: Option<i64> = r.get("file_mtime");
                let hash: String = r.get("content_hash");
                (sp, FileInfo { mtime_nanos: mt, content_hash: hash })
            })
            .collect()
    };

    let bar = ProgressBar::new(candidates.len() as u64);
    bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );

    let mut pending: Vec<DocRecord> = vec![];
    let mut total_chunks = 0usize;
    let mut skipped = 0usize;
    let mut incremental_skips = 0usize;

    for (abs_path, rel_path, doc_kind) in &candidates {
        bar.set_message(
            abs_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
        );

        // Cheap mtime stat — avoids reading the file at all when unchanged.
        let file_mtime: Option<i64> = std::fs::metadata(abs_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);

        if !force {
            if let Some(info) = existing.get(rel_path) {
                if let (Some(db_mt), Some(fs_mt)) = (info.mtime_nanos, file_mtime) {
                    if db_mt == fs_mt {
                        incremental_skips += 1;
                        bar.inc(1);
                        continue;
                    }
                }
            }
        }

        let source = match std::fs::read_to_string(abs_path) {
            Ok(s) => s,
            Err(_) => {
                skipped += 1;
                bar.inc(1);
                continue;
            }
        };

        let file_hash = sha256_hex(&source);

        if !force {
            if let Some(info) = existing.get(rel_path) {
                if info.content_hash == file_hash {
                    // Mtime drifted but content unchanged — update mtime for future fast path.
                    let _ = sqlx::query(
                        "UPDATE documents SET file_mtime = $1
                         WHERE repo_path = $2 AND source_path = $3",
                    )
                    .bind(file_mtime)
                    .bind(&repo_str)
                    .bind(rel_path)
                    .execute(pool)
                    .await;
                    incremental_skips += 1;
                    bar.inc(1);
                    continue;
                }
            }
        }

        let title = extract_title(&source, abs_path.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
        let chunks = chunk_markdown(&source);

        for chunk in chunks {
            if chunk.content.trim().len() < MIN_CHUNK_CHARS {
                continue;
            }
            let preview: String = chunk.content.split_whitespace().collect::<Vec<_>>().join(" ");
            let preview = &preview[..preview.len().min(280)];
            pending.push(DocRecord {
                repo_path: repo_str.clone(),
                source_path: rel_path.clone(),
                chunk_index: chunk.index,
                doc_kind: doc_kind.clone(),
                title: title.clone(),
                content: chunk.content,
                preview: preview.to_string(),
                content_hash: file_hash.clone(),
                file_mtime,
            });
            if pending.len() >= EMBED_BATCH {
                flush_docs(&mut pending, pool, &mut total_chunks).await?;
            }
        }
        bar.inc(1);
    }
    flush_docs(&mut pending, pool, &mut total_chunks).await?;
    bar.finish_and_clear();

    eprintln!(
        "Done. Indexed {} doc chunks ({} files unchanged, {} skipped).",
        total_chunks, incremental_skips, skipped
    );
    Ok(())
}

// ── Types ─────────────────────────────────────────────────────────────────────

struct DocRecord {
    repo_path: String,
    source_path: String,
    chunk_index: i32,
    doc_kind: String,
    title: String,
    content: String,
    preview: String,
    content_hash: String,
    file_mtime: Option<i64>,
}

struct MarkdownChunk {
    index: i32,
    content: String,
}

// ── File discovery ────────────────────────────────────────────────────────────

fn classify_doc(rel_path: &str) -> Option<&'static str> {
    let p = format!("/{}", rel_path.replace('\\', "/"));
    // Order matters: more specific patterns first
    if p.ends_with("/AGENTS.md") || p == "/AGENTS.md" {
        Some("agent_instruction")
    } else if p.ends_with("/CLAUDE.md") || p == "/CLAUDE.md" {
        Some("agent_instruction")
    } else if p.contains("/.agent/workflows/") && p.ends_with(".md") {
        Some("workflow")
    } else if p.contains("/.agent/skills/") && p.ends_with(".md") {
        Some("skill")
    } else if p.to_lowercase().contains("/.agent/sops/") && p.ends_with(".md") {
        Some("sop")
    } else if p.contains("/.agent/plans/") && p.ends_with(".md") {
        Some("plan")
    } else if p.to_lowercase().ends_with("/.agent/readme.md") {
        Some("agent_index")
    } else if p.to_lowercase().ends_with("/readme.md") || p.to_lowercase() == "/readme.md" {
        Some("readme")
    } else {
        None
    }
}

fn collect_docs(repo_path: &Path) -> Vec<(std::path::PathBuf, String, String)> {
    let mut result = vec![];

    let walker = ignore::WalkBuilder::new(repo_path)
        .standard_filters(true)
        .hidden(false)
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or("");
            !SKIP_DIRS.contains(&name)
        })
        .build();

    for entry in walker.flatten() {
        let path = entry.path().to_path_buf();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext.to_lowercase() != "md" {
            continue;
        }
        let rel = path
            .strip_prefix(repo_path)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if let Some(kind) = classify_doc(&rel) {
            result.push((path, rel, kind.to_string()));
        }
    }
    result
}

// ── Chunking ──────────────────────────────────────────────────────────────────

fn extract_title(text: &str, fallback: &str) -> String {
    for line in text.lines() {
        let line = line.trim();
        if let Some(title) = line.strip_prefix("# ") {
            return title.trim().to_string();
        }
    }
    fallback.to_string()
}

fn chunk_markdown(source: &str) -> Vec<MarkdownChunk> {
    if source.len() <= MAX_CHUNK_CHARS {
        return vec![MarkdownChunk { index: 0, content: source.to_string() }];
    }

    // Split on H2 headings
    let sections = split_on_heading(source, "## ");
    let mut chunks = vec![];
    let mut idx = 0i32;

    for section in sections {
        if section.trim().is_empty() {
            continue;
        }
        if section.len() <= MAX_CHUNK_CHARS {
            chunks.push(MarkdownChunk { index: idx, content: section.trim_end().to_string() });
            idx += 1;
        } else {
            // Further split on H3
            for sub in split_on_heading(&section, "### ") {
                if sub.trim().is_empty() {
                    continue;
                }
                let content = &sub[..sub.len().min(MAX_CHUNK_CHARS)];
                chunks.push(MarkdownChunk {
                    index: idx,
                    content: content.trim_end().to_string(),
                });
                idx += 1;
            }
        }
    }

    if chunks.is_empty() {
        chunks.push(MarkdownChunk {
            index: 0,
            content: source[..source.len().min(MAX_CHUNK_CHARS)].to_string(),
        });
    }
    chunks
}

/// Split text on lines that start with `prefix`, keeping prefix with each section.
fn split_on_heading(text: &str, prefix: &str) -> Vec<String> {
    let mut sections = vec![];
    let mut current = String::new();
    for line in text.lines() {
        if line.starts_with(prefix) && !current.trim().is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        sections.push(current);
    }
    if sections.is_empty() {
        sections.push(text.to_string());
    }
    sections
}

// ── Flush to DB ───────────────────────────────────────────────────────────────

async fn flush_docs(
    pending: &mut Vec<DocRecord>,
    pool: &PgPool,
    total: &mut usize,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let texts: Vec<&str> = pending.iter().map(|d| d.content.as_str()).collect();
    let embeddings = embed_batch(&texts).await?;

    for (rec, emb) in pending.iter().zip(embeddings.iter()) {
        sqlx::query(
            "INSERT INTO documents
                 (repo_path, source_path, chunk_index, doc_kind, title,
                  content, preview, content_hash, file_mtime, embedding)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::vector)
             ON CONFLICT (repo_path, source_path, chunk_index) DO UPDATE
                 SET doc_kind     = EXCLUDED.doc_kind,
                     title        = EXCLUDED.title,
                     content      = EXCLUDED.content,
                     preview      = EXCLUDED.preview,
                     content_hash = EXCLUDED.content_hash,
                     file_mtime   = EXCLUDED.file_mtime,
                     embedding    = EXCLUDED.embedding,
                     indexed_at   = NOW()",
        )
        .bind(&rec.repo_path)
        .bind(&rec.source_path)
        .bind(rec.chunk_index)
        .bind(&rec.doc_kind)
        .bind(&rec.title)
        .bind(&rec.content)
        .bind(&rec.preview)
        .bind(&rec.content_hash)
        .bind(rec.file_mtime)
        .bind(vec_literal(emb))
        .execute(pool)
        .await?;
    }

    *total += pending.len();
    pending.clear();
    Ok(())
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}
