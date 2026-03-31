use crate::db::{ChunkRow, DocRow, GithubRow};

// ── Code chunks ───────────────────────────────────────────────────────────────

pub fn fmt_chunks(rows: &[ChunkRow], scores: Option<&[f32]>, score_label: &str) -> String {
    if rows.is_empty() {
        return "No results found.".to_string();
    }
    let mut parts = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let score = scores
            .and_then(|s| s.get(i).copied())
            .or_else(|| r.rrf_score.map(|s| s as f32));

        let mut header = format!(
            "### {}:{}-{}",
            r.file_path,
            r.start_line.unwrap_or(0),
            r.end_line.unwrap_or(0)
        );
        if let Some(kind) = &r.symbol_kind {
            header += &format!("  [{kind}]");
        }
        if let Some(s) = score {
            header += &format!("  ({score_label}={s:.4})");
        }
        let lang = r.language.as_deref().unwrap_or("");
        parts.push(format!("{header}\n```{lang}\n{}\n```", r.content));
    }
    parts.join("\n\n")
}

// ── Documents ─────────────────────────────────────────────────────────────────

pub fn fmt_docs(rows: &[DocRow]) -> String {
    if rows.is_empty() {
        return "No results found.".to_string();
    }
    let mut parts = Vec::new();
    for r in rows {
        let kind = r.doc_kind.as_deref().unwrap_or("doc");
        let title = r.title.as_deref().unwrap_or(&r.source_path);
        let header = format!(
            "### [{kind}] {title}  (score={:.4})\n**Repo:** {}  **Path:** {}",
            r.rrf_score, r.repo_path, r.source_path
        );
        parts.push(format!("{header}\n\n{}", r.content));
    }
    parts.join("\n\n---\n\n")
}

// ── GitHub ────────────────────────────────────────────────────────────────────

const CONTENT_PREVIEW: usize = 600;

pub fn fmt_github(rows: &[GithubRow]) -> String {
    if rows.is_empty() {
        return "No results found.".to_string();
    }
    let mut parts = Vec::new();
    for r in rows {
        let kind = if r.entity_type == "pr" { "PR" } else { "Issue" };
        let state = r.state.as_deref().unwrap_or("?");
        let author = r.author.as_deref().unwrap_or("unknown");
        let created = r.created_at.as_deref().unwrap_or("");
        let labels = r.labels.as_deref().unwrap_or("");

        let mut header = format!(
            "### {kind} #{}: {}  [{}]  (score={:.4})",
            r.number, r.title, state, r.rrf_score
        );
        header += &format!("\n**Repo:** {}  **Author:** {}  **Created:** {}", r.repo, author, created);
        if !labels.is_empty() {
            header += &format!("  **Labels:** {labels}");
        }

        // Truncate body for display (skip the title which is already in header)
        let body = r
            .content
            .split_once("\n\n")
            .map(|(_, b)| b)
            .unwrap_or(&r.content);
        let preview = if body.len() > CONTENT_PREVIEW {
            format!("{}…", &body[..CONTENT_PREVIEW])
        } else {
            body.to_string()
        };

        parts.push(format!("{header}\n\n{preview}"));
    }
    parts.join("\n\n---\n\n")
}
