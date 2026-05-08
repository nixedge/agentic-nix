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

/// Ensure the embed model is available locally, pulling it from the registry if needed.
/// Logs progress to stderr. Should be called once before starting ingest.
pub async fn ensure_embed_model() -> Result<()> {
    let host = std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let model = std::env::var("EMBED_MODEL")
        .unwrap_or_else(|_| "hf.co/jinaai/jina-code-embeddings-1.5b-GGUF:Q8_0".into());

    // Probe with a minimal embed — fast if model is already loaded.
    let probe = client()
        .post(format!("{host}/api/embed"))
        .json(&serde_json::json!({"model": model, "input": [""]}))
        .timeout(Duration::from_secs(30))
        .send()
        .await;

    let needs_pull = match probe {
        Ok(r) if r.status().is_success() => return Ok(()), // already available
        Ok(r) if r.status().as_u16() == 404 => true,
        Ok(_) => true,  // any other error — try pulling anyway
        Err(_) => return Err(anyhow!("Ollama unreachable at {host}")),
    };

    if needs_pull {
        eprintln!("[agentic-nix] Embed model '{model}' not found locally — pulling from registry...");

        // Stream the pull so we can log progress milestones.
        let mut resp = client()
            .post(format!("{host}/api/pull"))
            .json(&serde_json::json!({"model": model, "stream": true}))
            .timeout(Duration::from_secs(1800)) // 30 min — large model
            .send()
            .await
            .context("Ollama pull request failed")?;

        let mut last_status = String::new();
        let mut buf = Vec::new();

        while let Some(chunk) = resp.chunk().await? {
            buf.extend_from_slice(&chunk);
            // Each newline-delimited JSON object is one progress event.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&line) {
                    let status = val["status"].as_str().unwrap_or("").to_string();
                    if status != last_status {
                        if let (Some(completed), Some(total)) =
                            (val["completed"].as_u64(), val["total"].as_u64())
                        {
                            let pct = completed * 100 / total.max(1);
                            eprintln!("  {status}: {pct}%");
                        } else {
                            eprintln!("  {status}");
                        }
                        last_status = status;
                    }
                }
            }
        }

        eprintln!("[agentic-nix] Pull complete: {model}");
    }

    // Check whether the model landed on GPU.
    if let Ok(resp) = client()
        .get(format!("{host}/api/ps"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        if let Ok(data) = resp.json::<serde_json::Value>().await {
            let on_cpu = data["models"].as_array().map(|ms| {
                ms.iter().any(|m| {
                    m["size"].as_u64().unwrap_or(0) > 0
                        && m["size_vram"].as_u64().unwrap_or(0) == 0
                })
            }).unwrap_or(false);
            if on_cpu {
                eprintln!(
                    "WARNING: Ollama embed model is running on CPU (size_vram=0). \
                     Indexing will be slow. Check GPU/CUDA configuration."
                );
            }
        }
    }

    Ok(())
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
