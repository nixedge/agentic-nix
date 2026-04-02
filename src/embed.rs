use anyhow::{Context, Result};
use reqwest::Client;
use std::sync::OnceLock;
use std::time::Duration;

static HTTP: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    HTTP.get_or_init(Client::new)
}

/// Timeout for query embedding. Must be shorter than the MCP client's request timeout
/// so we can return a descriptive error rather than a generic -32001 timeout.
/// Overridable via EMBED_TIMEOUT_SECS env var.
fn embed_timeout() -> Duration {
    let secs = std::env::var("EMBED_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20u64);
    Duration::from_secs(secs)
}

pub async fn embed(text: &str) -> Result<Vec<f32>> {
    let ollama_host =
        std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let embed_model = std::env::var("EMBED_MODEL")
        .unwrap_or_else(|_| "hf.co/jinaai/jina-code-embeddings-1.5b-GGUF:Q8_0".into());

    tracing::debug!(model = %embed_model, host = %ollama_host, "Embedding query via Ollama");

    let resp: serde_json::Value = client()
        .post(format!("{ollama_host}/api/embed"))
        .json(&serde_json::json!({
            "model": embed_model,
            "input": [text]
        }))
        .timeout(embed_timeout())
        .send()
        .await
        .with_context(|| {
            format!(
                "Ollama unreachable at {ollama_host} (is Ollama running? model: {embed_model})"
            )
        })?
        .error_for_status()
        .with_context(|| format!("Ollama returned error status (model: {embed_model})"))?
        .json()
        .await
        .context("Failed to parse Ollama embed response")?;

    let embeddings: Vec<f32> = resp["embeddings"][0]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no embeddings in response from {embed_model}"))?
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
        .collect();

    Ok(embeddings)
}
