use anyhow::{Context, Result};
use reqwest::Client;
use std::sync::OnceLock;

static HTTP: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    HTTP.get_or_init(Client::new)
}

/// Embed a batch of texts via Ollama /api/embed.
pub async fn embed_batch(texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let host = std::env::var("OLLAMA_HOST")
        .unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let model = std::env::var("EMBED_MODEL")
        .unwrap_or_else(|_| "hf.co/jinaai/jina-code-embeddings-1.5b-GGUF:Q8_0".into());

    let body = serde_json::json!({
        "model": model,
        "input": texts,
    });

    let resp = client()
        .post(format!("{host}/api/embed"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(90))
        .send()
        .await
        .context("Ollama request failed (is Ollama running?)")?;

    resp.error_for_status_ref()
        .context("Ollama returned error status")?;

    let data: serde_json::Value = resp.json().await?;
    let embeddings = data["embeddings"]
        .as_array()
        .context("missing 'embeddings' in Ollama response")?
        .iter()
        .map(|e| {
            e.as_array()
                .unwrap_or(&vec![])
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect()
        })
        .collect();

    Ok(embeddings)
}

/// Format a float vector for PostgreSQL vector literal.
pub fn vec_literal(v: &[f32]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}
