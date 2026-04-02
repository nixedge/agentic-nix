use anyhow::Result;
use reqwest::Client;
use std::sync::OnceLock;

static HTTP: OnceLock<Client> = OnceLock::new();

fn client() -> &'static Client {
    HTTP.get_or_init(Client::new)
}

pub async fn embed(text: &str) -> Result<Vec<f32>> {
    let ollama_host =
        std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let embed_model = std::env::var("EMBED_MODEL")
        .unwrap_or_else(|_| "hf.co/jinaai/jina-code-embeddings-1.5b-GGUF:Q8_0".into());

    let resp: serde_json::Value = client()
        .post(format!("{ollama_host}/api/embed"))
        .json(&serde_json::json!({
            "model": embed_model,
            "input": [text]
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let embeddings: Vec<f32> = resp["embeddings"][0]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no embeddings in response"))?
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0) as f32)
        .collect();

    Ok(embeddings)
}
