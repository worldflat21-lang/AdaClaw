/// OpenAI Embedding Provider
///
/// Uses the `text-embedding-3-small` model (1536 dimensions) by default.
/// Configurable model and base URL support self-hosted / proxy endpoints.
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::EmbeddingProvider;

/// Default model — small, cheap, high quality.
const DEFAULT_MODEL: &str = "text-embedding-3-small";
/// Default dimension for text-embedding-3-small
const DEFAULT_DIM: usize = 1536;

pub struct OpenAiEmbedProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
    dim: usize,
}

impl OpenAiEmbedProvider {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self::with_model(api_key, base_url, DEFAULT_MODEL.to_string(), DEFAULT_DIM)
    }

    pub fn with_model(api_key: String, base_url: String, model: String, dim: usize) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to create HTTP client"),
            api_key,
            base_url,
            model,
            dim,
        }
    }
}

// ── OpenAI REST types ────────────────────────────────────────────────────────

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: Vec<&'a str>,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
    index: usize,
}

// ── Trait impl ────────────────────────────────────────────────────────────────

#[async_trait]
impl EmbeddingProvider for OpenAiEmbedProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let body = EmbedRequest {
            input: texts.to_vec(),
            model: &self.model,
        };

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("OpenAI embedding request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "OpenAI embedding API error {}: {}",
                status,
                text
            ));
        }

        let mut response: EmbedResponse = resp
            .json()
            .await
            .context("Failed to parse OpenAI embedding response")?;

        // Sort by index to ensure order matches input
        response.data.sort_by_key(|d| d.index);
        Ok(response.data.into_iter().map(|d| d.embedding).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skipped by default — requires OPENAI_API_KEY env var and network access.
    #[tokio::test]
    #[ignore]
    async fn test_openai_embed() {
        let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY not set");
        let provider = OpenAiEmbedProvider::new(key, "https://api.openai.com/v1".to_string());
        assert_eq!(provider.dim(), DEFAULT_DIM);

        let embs = provider.embed(&["hello world"]).await.unwrap();
        assert_eq!(embs.len(), 1);
        assert_eq!(embs[0].len(), DEFAULT_DIM);
    }
}
