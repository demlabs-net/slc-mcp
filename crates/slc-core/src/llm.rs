//! LLM client abstraction — summarization/reasoning + embeddings.
//!
//! [`OllamaClient`] speaks the Ollama HTTP API (as the Python legacy did);
//! any other provider can implement [`LlmClient`] (e.g. the core Vassista
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
