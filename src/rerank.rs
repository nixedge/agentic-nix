use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
use std::sync::OnceLock;

static RERANKER: OnceLock<Option<TextRerank>> = OnceLock::new();

fn get_reranker() -> &'static Option<TextRerank> {
    RERANKER.get_or_init(|| {
        let enabled = std::env::var("RERANK_MODEL")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if !enabled {
            return None;
        }
        match TextRerank::try_new(RerankInitOptions::new(RerankerModel::BGERerankerBase)) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("reranker init failed: {e}");
                None
            }
        }
    })
}

/// Score (query, doc) pairs with a cross-encoder. Returns None if disabled or on error.
/// Runs in a blocking thread so the async event loop is not blocked.
/// The returned slice is in the same order as the input docs.
pub async fn rerank(query: &str, docs: &[String]) -> Option<Vec<f32>> {
    let query = query.to_string();
    let docs = docs.to_vec();

    tokio::task::spawn_blocking(move || {
        let reranker = get_reranker().as_ref()?;
        let doc_strs: Vec<&String> = docs.iter().collect();
        let results = reranker.rerank(&query, doc_strs, false, None).ok()?;
        // results are sorted by score desc; reconstruct original-order scores
        let mut scores = vec![0.0f32; docs.len()];
        for r in &results {
            scores[r.index] = r.score;
        }
        Some(scores)
    })
    .await
    .ok()
    .flatten()
}
