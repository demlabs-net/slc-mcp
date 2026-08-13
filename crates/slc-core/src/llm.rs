//! LLM client abstraction — summarization/reasoning + embeddings.
//!
//! Two HTTP providers (auto-selected, same as the Python legacy):
//! - [`LmStudioClient`] — **LM Studio** (OpenAI-compatible `/v1/…`), chosen
//!   when `LMSTUDIO_URL` is set. `google/gemma-4-e4b` for reasoning/agentic
//!   work, `text-embedding-nomic-embed-text-v1.5` for embeddings (defaults,
//!   overridable via `LMSTUDIO_MODEL` / `LMSTUDIO_EMBED_MODEL`).
//! - [`OllamaClient`] — Ollama HTTP API, used otherwise.
//!
//! Any other provider can implement [`LlmClient`] (e.g. the core Vassista
//! `LlmProvider` when embedded via the static lib).

use crate::error::{SlcError, SlcResult};
use async_trait::async_trait;
use serde_json::json;

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Free-form reasoning/summarization call.
    async fn reason(&self, prompt: &str) -> SlcResult<String>;
    /// Text embedding vector.
    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>>;
}

/// Delegation: `Arc<dyn LlmClient>` is itself a client (shared across the
/// engine components).
#[async_trait]
impl LlmClient for std::sync::Arc<dyn LlmClient> {
    async fn reason(&self, prompt: &str) -> SlcResult<String> {
        self.as_ref().reason(prompt).await
    }
    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>> {
        self.as_ref().generate_embedding(text).await
    }
}

/// LM Studio client — OpenAI-compatible endpoints (`/v1/chat/completions`,
/// `/v1/embeddings`). Selected when `LMSTUDIO_URL` is set.
///
/// Model defaults (user-approved):
/// - reasoning/agentic (compression, consolidation, later subagents/search):
///   `google/gemma-4-e4b`
/// - embeddings: `text-embedding-nomic-embed-text-v1.5`
#[derive(Clone)]
pub struct LmStudioClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    embed_model: String,
}

impl LmStudioClient {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, embed_model: impl Into<String>) -> Self {
        LmStudioClient {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            embed_model: embed_model.into(),
        }
    }

    /// Resolve from env: `LMSTUDIO_URL` (required), `LMSTUDIO_MODEL`
    /// (default `google/gemma-4-e4b`), `LMSTUDIO_EMBED_MODEL`
    /// (default `text-embedding-nomic-embed-text-v1.5`).
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("LMSTUDIO_URL").ok()?;
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        Some(Self::new(
            url,
            std::env::var("LMSTUDIO_MODEL").unwrap_or_else(|_| "google/gemma-4-e4b".into()),
            std::env::var("LMSTUDIO_EMBED_MODEL")
                .unwrap_or_else(|_| "text-embedding-nomic-embed-text-v1.5".into()),
        ))
    }
}

/// Request body builders (pure, unit-tested).
pub fn lmstudio_chat_body(model: &str, prompt: &str) -> serde_json::Value {
    json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    })
}

pub fn lmstudio_embed_body(model: &str, text: &str) -> serde_json::Value {
    json!({ "model": model, "input": text })
}

#[async_trait]
impl LlmClient for LmStudioClient {
    async fn reason(&self, prompt: &str) -> SlcResult<String> {
        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .bearer_auth("lm-studio")
            .json(&lmstudio_chat_body(&self.model, prompt))
            .send()
            .await
            .map_err(|e| SlcError::Llm(format!("lmstudio chat: {e}")))?;
        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SlcError::Llm(format!("lmstudio chat decode: {e}")))?;
        if !status.is_success() {
            return Err(SlcError::Llm(format!("lmstudio chat HTTP {status}: {json}")));
        }
        json.pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| SlcError::Llm("lmstudio chat: missing choices[0].message.content".into()))
    }

    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>> {
        let resp = self
            .http
            .post(format!("{}/v1/embeddings", self.base_url))
            .bearer_auth("lm-studio")
            .json(&lmstudio_embed_body(&self.embed_model, text))
            .send()
            .await
            .map_err(|e| SlcError::Llm(format!("lmstudio embeddings: {e}")))?;
        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SlcError::Llm(format!("lmstudio embeddings decode: {e}")))?;
        if !status.is_success() {
            return Err(SlcError::Llm(format!("lmstudio embeddings HTTP {status}: {json}")));
        }
        json.pointer("/data/0/embedding")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect())
            .ok_or_else(|| SlcError::Llm("lmstudio embeddings: missing data[0].embedding".into()))
    }
}

/// Ollama HTTP client (matches the legacy `ollama_client.py`).
#[derive(Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
    endpoint: String,
    reasoning_model: String,
    embedding_model: String,
}

impl OllamaClient {
    pub fn new(endpoint: impl Into<String>, reasoning_model: impl Into<String>, embedding_model: impl Into<String>) -> Self {
        OllamaClient {
            http: reqwest::Client::new(),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            reasoning_model: reasoning_model.into(),
            embedding_model: embedding_model.into(),
        }
    }

    /// Defaults from env (`OLLAMA_ENDPOINT`, `OLLAMA_REASONING_MODEL`,
    /// `OLLAMA_EMBEDDING_MODEL`) with the legacy fallbacks.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var("OLLAMA_ENDPOINT").unwrap_or_else(|_| "http://localhost:11434".into()),
            std::env::var("OLLAMA_REASONING_MODEL").unwrap_or_else(|_| "gemma3:latest".into()),
            std::env::var("OLLAMA_EMBEDDING_MODEL").unwrap_or_else(|_| "bge-m3".into()),
        )
    }
}

#[async_trait]
impl LlmClient for OllamaClient {
    async fn reason(&self, prompt: &str) -> SlcResult<String> {
        let body = json!({ "model": self.reasoning_model, "prompt": prompt, "stream": false });
        let resp = self
            .http
            .post(format!("{}/api/generate", self.endpoint))
            .json(&body)
            .send()
            .await
            .map_err(|e| SlcError::Llm(format!("ollama generate: {e}")))?;
        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SlcError::Llm(format!("ollama generate decode: {e}")))?;
        if !status.is_success() {
            return Err(SlcError::Llm(format!("ollama generate HTTP {status}: {json}")));
        }
        json.get("response")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| SlcError::Llm("ollama generate: missing `response`".into()))
    }

    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>> {
        let body = json!({ "model": self.embedding_model, "prompt": text });
        let resp = self
            .http
            .post(format!("{}/api/embeddings", self.endpoint))
            .json(&body)
            .send()
            .await
            .map_err(|e| SlcError::Llm(format!("ollama embeddings: {e}")))?;
        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SlcError::Llm(format!("ollama embeddings decode: {e}")))?;
        if !status.is_success() {
            return Err(SlcError::Llm(format!("ollama embeddings HTTP {status}: {json}")));
        }
        json.get("embedding")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect())
            .ok_or_else(|| SlcError::Llm("ollama embeddings: missing `embedding`".into()))
    }
}

/// Deterministic in-memory client for tests and offline operation.
/// `reason` returns the prompt echo or a canned script; `generate_embedding`
/// returns a hash-based vector (stable per text).
#[derive(Clone, Default)]
pub struct MockLlm {
    /// Scripted responses returned in order (reason).
    pub script: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl MockLlm {
    pub fn new(script: Vec<String>) -> Self {
        MockLlm { script: std::sync::Arc::new(std::sync::Mutex::new(script)) }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn reason(&self, prompt: &str) -> SlcResult<String> {
        let mut script = self.script.lock().unwrap();
        if !script.is_empty() {
            return Ok(script.remove(0));
        }
        // Default: echo first line of the prompt (deterministic, testable).
        Ok(prompt.lines().next().unwrap_or("").to_string())
    }

    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(text.as_bytes());
        let digest = h.finalize();
        Ok(digest.iter().take(16).map(|b| *b as f32 / 255.0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_body_shape() {
        let body = lmstudio_chat_body("google/gemma-4-e4b", "summarize");
        assert_eq!(body["model"], "google/gemma-4-e4b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "summarize");
    }

    #[test]
    fn embed_body_shape() {
        let body = lmstudio_embed_body("text-embedding-nomic-embed-text-v1.5", "hello");
        assert_eq!(body["model"], "text-embedding-nomic-embed-text-v1.5");
        assert_eq!(body["input"], "hello");
    }
}
