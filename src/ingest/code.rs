use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;
use tokio::task::JoinSet;

use super::embed::{embed_batch, vec_literal};
use super::symbols::extract_symbols;

const EMBED_BATCH: usize = 16;
const MAX_CONCURRENT_FLUSHES: usize = 4;
const CHUNK_LINES: usize = 120;
const OVERLAP_LINES: usize = 15;
const MAX_CHUNK_BYTES: usize = 8_000;
const MAX_CHUNKS_PER_FILE: usize = 256;

const CODE_EXTENSIONS: &[&str] = &[
    "hs", "rs", "py", "ts", "tsx", "js", "jsx", "nix", "go", "java", "scala", "ml", "mli", "c",
    "cpp", "h", "hpp", "sql", "sh", "toml", "yaml", "yml", "json", "tex", "cabal",
];

const SKIP_DIRS: &[&str] = &[
    "target",
    "dist",
    "build",
    "__pycache__",
    ".stack-work",
    "vendor",
    ".cache",
    "result",
    ".direnv",
    "coverage",
    ".next",
    "out",
];

pub async fn ingest_code(
    pool: &PgPool,
    repo_path: &Path,
    force: bool,
    extra_patterns: &[String],
    repo_path_override: Option<&str>,
) -> Result<()> {
    let repo_str = match repo_path_override {
        Some(s) => s.to_string(),
        None => repo_path.canonicalize()?.to_string_lossy().into_owned(),
    };

    // Collect files
    let all_files = collect_files(repo_path, extra_patterns);
    if all_files.is_empty() {
        eprintln!("No files matched.");
        return Ok(());
    }
    eprintln!("Found {} files in {}", all_files.len(), repo_str);

    // Optionally clear existing data
    if force {
        let n: i64 = sqlx::query_scalar(
            "WITH d AS (DELETE FROM code_chunks WHERE repo_path = $1 RETURNING 1)
             SELECT COUNT(*) FROM d",
        )
        .bind(&repo_str)
        .fetch_one(pool)
        .await
        .unwrap_or(0);
        eprintln!("Cleared {} existing chunks.", n);
    }

    // Load existing (file_path → (mtime_nanos, content_hash)) for incremental skip.
    // mtime is used as a cheap stat-based pre-check; hash is the fallback for correctness.
    struct FileInfo {
        mtime_nanos: Option<i64>,
        content_hash: String,
    }
    let existing: HashMap<String, FileInfo> = if force {
        HashMap::new()
    } else {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (file_path) file_path, file_mtime, content_hash
             FROM code_chunks
             WHERE repo_path = $1 AND content_hash IS NOT NULL
             ORDER BY file_path, chunk_index",
        )
        .bind(&repo_str)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        rows.iter()
            .map(|r| {
                use sqlx::Row as _;
                let fp: String = r.get("file_path");
                let mt: Option<i64> = r.get("file_mtime");
                let hash: String = r.get("content_hash");
                (
                    fp,
                    FileInfo {
                        mtime_nanos: mt,
                        content_hash: hash,
                    },
                )
            })
            .collect()
    };

    let bar = ProgressBar::new(all_files.len() as u64);
    bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("=>-"),
    );

    let mut pending: Vec<ChunkRecord> = Vec::with_capacity(EMBED_BATCH);
    let mut tasks: JoinSet<Result<usize>> = JoinSet::new();
    let mut total_chunks = 0usize;
    let mut skipped_files = 0usize;
    let mut incremental_skips = 0usize;

    for file_path in &all_files {
        bar.set_message(
            file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
        );

        let rel = file_path
            .strip_prefix(repo_path)
            .unwrap_or(file_path)
            .to_string_lossy()
            .into_owned();

        // Cheap mtime stat — avoids reading the file at all when unchanged.
        let file_mtime: Option<i64> = std::fs::metadata(file_path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64);

        if !force {
            if let Some(info) = existing.get(&rel) {
                if let (Some(db_mt), Some(fs_mt)) = (info.mtime_nanos, file_mtime) {
                    if db_mt == fs_mt {
                        // Fast path: mtime matches — skip without reading.
                        incremental_skips += 1;
                        bar.inc(1);
                        continue;
                    }
                }
            }
        }

        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(_) => {
                skipped_files += 1;
                bar.inc(1);
                continue;
            }
        };

        let file_hash = sha256_hex(&source);

        if !force {
            if let Some(info) = existing.get(&rel) {
                if info.content_hash == file_hash {
                    // Hash matches but mtime differed (e.g. touch/copy). Update mtime so
                    // the next run can use the fast path again.
                    let _ = sqlx::query(
                        "UPDATE code_chunks SET file_mtime = $1
                         WHERE repo_path = $2 AND file_path = $3",
                    )
                    .bind(file_mtime)
                    .bind(&repo_str)
                    .bind(&rel)
                    .execute(pool)
                    .await;
                    incremental_skips += 1;
                    bar.inc(1);
                    continue;
                }
            }
        }

        let lang = detect_language(file_path);
        let chunks = make_chunks(&source, &lang);

        for chunk in chunks {
            pending.push(ChunkRecord {
                repo_path: repo_str.clone(),
                file_path: rel.clone(),
                chunk_index: chunk.index,
                content: chunk.content,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                language: lang.clone(),
                symbol_kind: chunk.symbol_kind,
                content_hash: file_hash.clone(),
                file_mtime,
            });
            if pending.len() >= EMBED_BATCH {
                // Throttle: wait for one task before spawning another if at capacity.
                if tasks.len() >= MAX_CONCURRENT_FLUSHES {
                    if let Some(res) = tasks.join_next().await {
                        total_chunks += res??;
                    }
                }
                let batch = std::mem::replace(&mut pending, Vec::with_capacity(EMBED_BATCH));
                let pool2 = pool.clone();
                tasks.spawn(async move { flush_bulk(batch, pool2).await });
            }
        }
        bar.inc(1);
    }

    // Flush remainder then drain all in-flight tasks.
    if !pending.is_empty() {
        let pool2 = pool.clone();
        tasks.spawn(async move { flush_bulk(pending, pool2).await });
    }
    while let Some(res) = tasks.join_next().await {
        total_chunks += res??;
    }
    bar.finish_and_clear();

    eprintln!(
        "Done. Indexed {} chunks ({} files unchanged, {} skipped).",
        total_chunks, incremental_skips, skipped_files
    );
    Ok(())
}

// ── Internal types ────────────────────────────────────────────────────────────

struct Chunk {
    index: i32,
    content: String,
    start_line: i32,
    end_line: i32,
    symbol_kind: Option<String>,
}

struct ChunkRecord {
    repo_path: String,
    file_path: String,
    chunk_index: i32,
    content: String,
    start_line: i32,
    end_line: i32,
    language: String,
    symbol_kind: Option<String>,
    content_hash: String,
    file_mtime: Option<i64>,
}

// ── File collection ───────────────────────────────────────────────────────────

fn collect_files(repo_path: &Path, extra_patterns: &[String]) -> Vec<std::path::PathBuf> {
    let mut files = vec![];

    let walker = ignore::WalkBuilder::new(repo_path)
        .standard_filters(true) // respects .gitignore, .ignore
        .hidden(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_str().unwrap_or("");
            !SKIP_DIRS.contains(&name)
        })
        .build();

    for entry in walker.flatten() {
        let path = entry.path().to_path_buf();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_cabal_project = filename == "cabal.project"
            || filename == "cabal.project.freeze"
            || filename == "cabal.project.local";

        let matches = CODE_EXTENSIONS.contains(&ext.as_str())
            || is_cabal_project
            || extra_patterns.iter().any(|p| {
                glob::Pattern::new(p)
                    .map(|pat| pat.matches_path(&path))
                    .unwrap_or(false)
            });

        if matches {
            files.push(path);
        }
    }
    files
}

// ── Language detection ────────────────────────────────────────────────────────

fn detect_language(path: &Path) -> String {
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if filename == "cabal.project"
        || filename == "cabal.project.freeze"
        || filename == "cabal.project.local"
    {
        return "cabal".to_string();
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "hs" => "haskell",
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" => "javascript",
        "nix" => "nix",
        "go" => "go",
        "java" => "java",
        "scala" => "scala",
        "ml" | "mli" => "ocaml",
        "c" | "h" => "c",
        "cpp" | "hpp" => "cpp",
        "sql" => "sql",
        "tex" => "latex",
        "sh" => "bash",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "cabal" => "cabal",
        _ => "text",
    }
    .to_string()
}

// ── Chunking ──────────────────────────────────────────────────────────────────

const SYMBOL_LANGUAGES: &[&str] = &[
    "typescript",
    "javascript",
    "python",
    "rust",
    "haskell",
    "latex",
    "nix",
];

fn make_chunks(source: &str, language: &str) -> Vec<Chunk> {
    if !SYMBOL_LANGUAGES.contains(&language) {
        return chunk_lines(source);
    }

    let syms = extract_symbols(source, language);
    if syms.is_empty() {
        return chunk_lines(source);
    }

    let total_lines = source.lines().count();
    let max_covered = syms.iter().map(|s| s.end_line as usize).max().unwrap_or(0);

    let mut chunks: Vec<Chunk> = syms
        .into_iter()
        .take(MAX_CHUNKS_PER_FILE)
        .enumerate()
        .map(|(i, s)| Chunk {
            index: i as i32,
            content: s.content,
            start_line: s.start_line as i32,
            end_line: s.end_line as i32,
            symbol_kind: Some(s.kind),
        })
        .collect();

    // If the parser only covered a fraction of the file (e.g. tikzpicture or
    // other constructs that confuse the grammar), fill in the uncovered tail
    // with overlapping line windows so nothing is silently dropped.
    if max_covered + CHUNK_LINES * 2 < total_lines && chunks.len() < MAX_CHUNKS_PER_FILE {
        let lines: Vec<&str> = source.lines().collect();
        let step = CHUNK_LINES.saturating_sub(OVERLAP_LINES).max(1);
        let start_from = max_covered.saturating_sub(OVERLAP_LINES);
        let mut i = start_from;
        let mut idx = chunks.len() as i32;
        while i < total_lines && chunks.len() < MAX_CHUNKS_PER_FILE {
            let end = (i + CHUNK_LINES).min(total_lines);
            let text = lines[i..end].join("\n");
            if !text.trim().is_empty() && text.len() <= MAX_CHUNK_BYTES {
                chunks.push(Chunk {
                    index: idx,
                    content: text,
                    start_line: (i + 1) as i32,
                    end_line: end as i32,
                    symbol_kind: None,
                });
                idx += 1;
            }
            i += step;
        }
    }

    chunks
}

fn chunk_lines(source: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = source.lines().collect();
    let step = CHUNK_LINES.saturating_sub(OVERLAP_LINES).max(1);
    let mut chunks = vec![];

    let mut i = 0usize;
    let mut chunk_idx = 0i32;
    while i < lines.len() && chunks.len() < MAX_CHUNKS_PER_FILE {
        let end = (i + CHUNK_LINES).min(lines.len());
        let text = lines[i..end].join("\n");
        if text.trim().is_empty() || text.len() > MAX_CHUNK_BYTES {
            i += step;
            continue;
        }
        chunks.push(Chunk {
            index: chunk_idx,
            content: text,
            start_line: (i + 1) as i32,
            end_line: end as i32,
            symbol_kind: None,
        });
        chunk_idx += 1;
        if end >= lines.len() {
            break;
        }
        i += step;
    }
    chunks
}

// ── Flush batch to DB ─────────────────────────────────────────────────────────

/// Embed a batch of chunks and write them to the DB in a single bulk INSERT … UNNEST(…).
/// Returns the number of rows upserted.
async fn flush_bulk(records: Vec<ChunkRecord>, pool: PgPool) -> Result<usize> {
    if records.is_empty() {
        return Ok(0);
    }

    let texts: Vec<&str> = records.iter().map(|r| r.content.as_str()).collect();
    let embeddings = embed_batch(&texts).await?;

    // Build typed arrays for UNNEST — one allocation per column, one round-trip total.
    let repo_paths:    Vec<&str>          = records.iter().map(|r| r.repo_path.as_str()).collect();
    let file_paths:    Vec<&str>          = records.iter().map(|r| r.file_path.as_str()).collect();
    let indices:       Vec<i32>           = records.iter().map(|r| r.chunk_index).collect();
    let contents:      Vec<&str>          = records.iter().map(|r| r.content.as_str()).collect();
    let start_lines:   Vec<i32>           = records.iter().map(|r| r.start_line).collect();
    let end_lines:     Vec<i32>           = records.iter().map(|r| r.end_line).collect();
    let languages:     Vec<&str>          = records.iter().map(|r| r.language.as_str()).collect();
    let symbol_kinds:  Vec<Option<&str>>  = records.iter().map(|r| r.symbol_kind.as_deref()).collect();
    let hashes:        Vec<&str>          = records.iter().map(|r| r.content_hash.as_str()).collect();
    let mtimes:        Vec<Option<i64>>   = records.iter().map(|r| r.file_mtime).collect();
    let vecs:          Vec<String>        = embeddings.iter().map(|e| vec_literal(e)).collect();

    let n = records.len();

    sqlx::query(
        "INSERT INTO code_chunks
             (repo_path, file_path, chunk_index, content,
              start_line, end_line, language, symbol_kind,
              content_hash, file_mtime, embedding)
         SELECT
             UNNEST($1::text[]),  UNNEST($2::text[]),  UNNEST($3::int4[]),
             UNNEST($4::text[]),  UNNEST($5::int4[]),  UNNEST($6::int4[]),
             UNNEST($7::text[]),  UNNEST($8::text[]),  UNNEST($9::text[]),
             UNNEST($10::int8[]), UNNEST($11::text[])::vector
         ON CONFLICT (repo_path, file_path, chunk_index) DO UPDATE SET
             content      = EXCLUDED.content,
             language     = EXCLUDED.language,
             symbol_kind  = EXCLUDED.symbol_kind,
             content_hash = EXCLUDED.content_hash,
             file_mtime   = EXCLUDED.file_mtime,
             embedding    = EXCLUDED.embedding,
             indexed_at   = NOW()",
    )
    .bind(repo_paths)
    .bind(file_paths)
    .bind(indices)
    .bind(contents)
    .bind(start_lines)
    .bind(end_lines)
    .bind(languages)
    .bind(symbol_kinds)
    .bind(hashes)
    .bind(mtimes)
    .bind(vecs)
    .execute(&pool)
    .await?;

    Ok(n)
}


// ── Utilities ─────────────────────────────────────────────────────────────────

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── chunk_lines ────────────────────────────────────────────────────────────

    #[test]
    fn chunk_lines_empty() {
        assert!(chunk_lines("").is_empty());
    }

    #[test]
    fn chunk_lines_short_file() {
        let src = "line1\nline2\nline3\n";
        let chunks = chunk_lines(src);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        assert!(chunks[0].content.contains("line1"));
        assert!(chunks[0].content.contains("line3"));
    }

    #[test]
    fn chunk_lines_respects_cap() {
        // Build a file with more lines than MAX_CHUNKS_PER_FILE * CHUNK_LINES
        // so we would exceed the cap if it weren't enforced.
        let line = "x = 1\n";
        let n = MAX_CHUNKS_PER_FILE * CHUNK_LINES + 200;
        let src = line.repeat(n);
        let chunks = chunk_lines(&src);
        assert!(
            chunks.len() <= MAX_CHUNKS_PER_FILE,
            "got {} chunks, expected <= {}",
            chunks.len(),
            MAX_CHUNKS_PER_FILE
        );
    }

    #[test]
    fn chunk_lines_overlap_means_consecutive_chunks_share_lines() {
        let lines: Vec<String> = (1..=300).map(|i| format!("line{i}")).collect();
        let src = lines.join("\n");
        let chunks = chunk_lines(&src);
        assert!(chunks.len() >= 2, "expected multiple chunks");
        // The end of chunk 0 should be >= the start of chunk 1 (overlap).
        assert!(
            chunks[0].end_line >= chunks[1].start_line,
            "no overlap between chunk 0 (end {}) and chunk 1 (start {})",
            chunks[0].end_line,
            chunks[1].start_line
        );
    }

    // ── make_chunks ────────────────────────────────────────────────────────────

    #[test]
    fn make_chunks_unknown_language_falls_back_to_lines() {
        let src = "hello\nworld\n";
        let chunks = make_chunks(src, "cobol");
        assert!(!chunks.is_empty());
        assert!(chunks[0].symbol_kind.is_none());
    }

    #[test]
    fn make_chunks_haskell_extracts_instances() {
        let src = r#"module Foo where

data Color = Red | Green | Blue

instance Show Color where
    show Red   = "Red"
    show Green = "Green"
    show Blue  = "Blue"
"#;
        let chunks = make_chunks(src, "haskell");
        let has_instance = chunks
            .iter()
            .any(|c| c.symbol_kind.as_deref() == Some("impl") && c.content.contains("Show Color"));
        assert!(
            has_instance,
            "expected an impl chunk for 'instance Show Color'"
        );
    }

    #[test]
    fn make_chunks_haskell_cpp_instance_is_extracted() {
        // An instance declaration wrapped in a CPP conditional block should not
        // be silently dropped by the Haskell symbol extractor.
        let src = r#"module Foo where

#if MIN_VERSION_base(4,10,0)
instance Show Foo where
    show _ = "Foo (new)"
#else
instance Show Foo where
    show _ = "Foo (old)"
#endif
"#;
        let chunks = make_chunks(src, "haskell");
        let has_instance = chunks
            .iter()
            .any(|c| c.symbol_kind.as_deref() == Some("impl") && c.content.contains("Show Foo"));
        assert!(
            has_instance,
            "instance inside CPP block was silently dropped"
        );
    }
}
