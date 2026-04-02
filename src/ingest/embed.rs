use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::time::sleep;

static HTTP: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    HTTP.get_or_init(Client::new)
}

const MAX_RETRIES: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);

/// Embed a batch of texts via Ollama /api/embed.
/// Retries up to MAX_RETRIES times with exponential backoff on transient errors.
pub async fn embed_batch(texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let model = std::env::var("EMBED_MODEL")
        .unwrap_or_else(|_| "hf.co/jinaai/jina-code-embeddings-1.5b-GGUF:Q8_0".into());

    let body = serde_json::json!({
        "model": model,
        "input": texts,
    });

    let mut last_err = anyhow!("no attempts made");
    let url = format!("{host}/api/embed");

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = INITIAL_BACKOFF * 2u32.pow(attempt - 1);
            tracing::warn!(
                attempt,
                backoff_secs = backoff.as_secs(),
                "Ollama request failed, retrying..."
            );
            sleep(backoff).await;
        }

        let send_result = client()
            .post(&url)
            .json(&body)
            .timeout(Duration::from_secs(120))
            .send()
            .await;

        let resp = match send_result {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                last_err = anyhow!(e).context("Ollama request failed (is Ollama running?)");
                continue;
            }
            Err(e) => return Err(anyhow!(e).context("Ollama request failed (is Ollama running?)")),
        };

        if let Err(e) = resp.error_for_status_ref() {
            return Err(anyhow!(e).context("Ollama returned error status"));
        }

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

        return Ok(embeddings);
    }

    Err(last_err.context(format!("Ollama request failed after {MAX_RETRIES} retries")))
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
