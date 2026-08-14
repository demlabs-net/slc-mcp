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
use serde_json::{Value, json};

/// Роль текста при эмбеддинге. Некоторые модели (e5-семейство) требуют
/// префиксов `query:`/`passage:` — запросы и документы кодируются по-разному.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingKind {
    /// Пользовательский запрос (поисковый).
    Query,
    /// Индексируемый документ/факт.
    Passage,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Free-form reasoning/summarization call.
    async fn reason(&self, prompt: &str) -> SlcResult<String>;
    /// Text embedding vector.
    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>>;
    /// Embedding with the text role; default is the plain call. Overridden by
    /// models that distinguish queries from passages (e5-style prefixes).
    async fn generate_embedding_kind(
        &self,
        text: &str,
        _kind: EmbeddingKind,
    ) -> SlcResult<Vec<f32>> {
        self.generate_embedding(text).await
    }
    /// Batch embedding (indexing/refresh/reindex paths). Default: sequential
    /// per-text calls; on-device clients (candle) override with a single
    /// batched forward — much faster for many short documents.
    async fn generate_embeddings(
        &self,
        texts: &[String],
        kind: EmbeddingKind,
    ) -> SlcResult<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.generate_embedding_kind(t, kind).await?);
        }
        Ok(out)
    }
    /// Human-readable name of the embedding model (logs, record metadata).
    fn embedding_model_name(&self) -> String {
        "unknown".into()
    }
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
    async fn generate_embedding_kind(
        &self,
        text: &str,
        kind: EmbeddingKind,
    ) -> SlcResult<Vec<f32>> {
        self.as_ref().generate_embedding_kind(text, kind).await
    }
    async fn generate_embeddings(
        &self,
        texts: &[String],
        kind: EmbeddingKind,
    ) -> SlcResult<Vec<Vec<f32>>> {
        self.as_ref().generate_embeddings(texts, kind).await
    }
    fn embedding_model_name(&self) -> String {
        self.as_ref().embedding_model_name()
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
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        embed_model: impl Into<String>,
    ) -> Self {
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
            return Err(SlcError::Llm(format!(
                "lmstudio chat HTTP {status}: {json}"
            )));
        }
        json.pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                SlcError::Llm("lmstudio chat: missing choices[0].message.content".into())
            })
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
            return Err(SlcError::Llm(format!(
                "lmstudio embeddings HTTP {status}: {json}"
            )));
        }
        json.pointer("/data/0/embedding")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                    .collect()
            })
            .ok_or_else(|| SlcError::Llm("lmstudio embeddings: missing data[0].embedding".into()))
    }

    fn embedding_model_name(&self) -> String {
        self.embed_model.clone()
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
    pub fn new(
        endpoint: impl Into<String>,
        reasoning_model: impl Into<String>,
        embedding_model: impl Into<String>,
    ) -> Self {
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
            return Err(SlcError::Llm(format!(
                "ollama generate HTTP {status}: {json}"
            )));
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
            return Err(SlcError::Llm(format!(
                "ollama embeddings HTTP {status}: {json}"
            )));
        }
        json.get("embedding")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                    .collect()
            })
            .ok_or_else(|| SlcError::Llm("ollama embeddings: missing `embedding`".into()))
    }

    fn embedding_model_name(&self) -> String {
        self.embedding_model.clone()
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
        MockLlm {
            script: std::sync::Arc::new(std::sync::Mutex::new(script)),
        }
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
        Ok(cpu_hash_embedding(text))
    }

    fn embedding_model_name(&self) -> String {
        "mock".into()
    }
}

/// LlmClient, который запрашивает инференс у MCP-КЛИЕНТА через sampling —
/// фоллбэк для машин без локального GPU/LLM-сервера (слабые компы,
/// виртуалки). Модель-клиента делает вызов за сервер; эмбеддинги через
/// sampling невозможны — `generate_embedding` возвращает ошибку, и поиск
/// честно деградирует в text-only (BM25).
pub struct McpSamplingLlm {
    /// Sampling-запросы наружу (сервер пересылает их клиенту по SSE).
    pub outbound: tokio::sync::mpsc::Sender<Value>,
    /// Ожидающие ответа запросы (id → канал ответа). Общий с сервером.
    pub pending: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<String>>>,
    >,
    max_tokens: usize,
}

impl McpSamplingLlm {
    pub fn new(
        outbound: tokio::sync::mpsc::Sender<Value>,
        pending: std::sync::Arc<
            std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<String>>>,
        >,
    ) -> Self {
        Self {
            outbound,
            pending,
            max_tokens: 2000,
        }
    }
}

#[async_trait]
impl LlmClient for McpSamplingLlm {
    async fn reason(&self, prompt: &str) -> SlcResult<String> {
        let request_id = format!("smp-{}", uuid::Uuid::new_v4());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        self.pending.lock().unwrap().insert(request_id.clone(), tx);
        let msg = json!({
            "type": "sampling_request",
            "request_id": request_id.clone(),
            "message": {
                "jsonrpc": "2.0",
                "id": request_id.clone(),
                "method": "sampling/createMessage",
                "params": {
                    "messages": [ { "role": "user", "content": { "type": "text", "text": prompt } } ],
                    "maxTokens": self.max_tokens,
                    "systemPrompt": "Ты — встроенный LLM SLC (память агента). Отвечай кратко и по делу.",
                },
            },
        });
        self.outbound
            .send(msg)
            .await
            .map_err(|_| SlcError::Storage("sampling channel closed".into()))?;
        match tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv()).await {
            Ok(Some(text)) => Ok(text),
            Ok(None) => Err(SlcError::Storage("sampling channel closed".into())),
            Err(_) => Err(SlcError::Storage(
                "sampling timed out — is the MCP client connected?".into(),
            )),
        }
    }

    async fn generate_embedding(&self, _text: &str) -> SlcResult<Vec<f32>> {
        Err(SlcError::Storage(
            "sampling client cannot embed — text-only search fallback".into(),
        ))
    }

    fn embedding_model_name(&self) -> String {
        "mcp-sampling".into()
    }
}

/// Deterministic lexical embedding: char n-grams (2..=4) hashed into a
/// 512-dim vector, L2-normalized. Shared by the CPU-hash fallback
/// (`CpuHashLlm`) and the test mock (`MockLlm`) — the mock must be
/// representative of a real embedder, or relevance tests lie (a naive
/// positive-only hash correlates ~0.5 with ANY text and drowns the
/// relevance gate in noise).
fn cpu_hash_embedding(text: &str) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let dim = 512usize;
    let mut out = vec![0.0f32; dim];
    // char n-grams (2..=4) hashed into the vector — cheap lexical signal.
    let chars: Vec<char> = text.chars().collect();
    for n in 2..=4 {
        for w in chars.windows(n) {
            let mut h = DefaultHasher::new();
            w.hash(&mut h);
            let idx = (h.finish() % dim as u64) as usize;
            out[idx] += 1.0;
        }
    }
    // L2-normalize.
    let norm = out.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-6);
    for v in out.iter_mut() {
        *v /= norm;
    }
    out
}

/// CPU-only fallback LLM for weak machines/virtual machines: embeddings
/// are deterministic hash features (n-gram → 512-dim) so hybrid search
/// works without ANY model server; reasoning returns a clear error (the
/// caller falls back to sampling or skips). Enable with `SLC_LLM=hash`.
pub struct CpuHashLlm;

#[async_trait]
impl LlmClient for CpuHashLlm {
    async fn reason(&self, _prompt: &str) -> SlcResult<String> {
        Err(SlcError::Storage(
            "no local LLM: enable SLC_MCP_SAMPLING for client inference or point SLC_LLM at Ollama/LM Studio".into(),
        ))
    }

    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>> {
        Ok(cpu_hash_embedding(text))
    }

    fn embedding_model_name(&self) -> String {
        "cpu-hash".into()
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

    #[tokio::test]
    async fn sampling_llm_roundtrip() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let llm = McpSamplingLlm::new(tx, pending.clone());
        // Simulate the MCP client: read the sampling request, answer it.
        let pending2 = pending.clone();
        tokio::spawn(async move {
            let msg = rx.recv().await.unwrap();
            let rid = msg["request_id"].as_str().unwrap().to_string();
            let tx = pending2.lock().unwrap().remove(&rid).unwrap();
            tx.send("сжатый ответ клиента".into()).await.unwrap();
        });
        let out = llm.reason("длинный текст для сжатия").await.unwrap();
        assert_eq!(out, "сжатый ответ клиента");
    }

    #[tokio::test]
    async fn cpu_hash_embeddings_are_deterministic_and_normalized() {
        let a = CpuHashLlm.generate_embedding("кофе чёрный").await.unwrap();
        let b = CpuHashLlm.generate_embedding("кофе чёрный").await.unwrap();
        let c = CpuHashLlm
            .generate_embedding("совсем другой текст")
            .await
            .unwrap();
        assert_eq!(a.len(), 512);
        assert_eq!(a, b);
        assert_ne!(a, c);
        let norm: f32 = a.iter().map(|v| v * v).sum();
        assert!((norm - 1.0).abs() < 1e-3);
        // Deterministic lexical similarity: similar texts correlate > 0.
        let dot: f32 = a.iter().zip(c.iter()).map(|(x, y)| x * y).sum();
        assert!(
            dot < 0.5,
            "dissimilar texts should be weakly correlated: {dot}"
        );
    }

    #[test]
    fn embed_body_shape() {
        let body = lmstudio_embed_body("text-embedding-nomic-embed-text-v1.5", "hello");
        assert_eq!(body["model"], "text-embedding-nomic-embed-text-v1.5");
        assert_eq!(body["input"], "hello");
    }
}
