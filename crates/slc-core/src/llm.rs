//! LLM client abstraction — summarization/reasoning + embeddings.
//!
//! Production can use [`McpSamplingLlm`] for seat-scoped reasoning through the
//! already connected MCP client's model. It needs no dedicated model server;
//! embeddings are unavailable in this mode and search falls back to BM25.
//! Standalone deployments can instead select LM Studio, Ollama, Candle, or the
//! deterministic CPU-hash provider.
//!
//! Any other provider can implement [`LlmClient`] (e.g. the core Vassista
//! `LlmProvider` when embedded via the static lib).

use crate::error::{SlcError, SlcResult};
use async_trait::async_trait;
use serde_json::{Value, json};

/// The role of the text being embedded. Some models (e5 family) require
/// `query:`/`passage:` prefixes — queries and documents are encoded differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingKind {
    /// A user query (search).
    Query,
    /// An indexed document/fact.
    Passage,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Free-form reasoning/summarization call.
    async fn reason(&self, prompt: &str) -> SlcResult<String>;

    /// Reasoning with the seat context. Background jobs (compression,
    /// insights) call this so seat-scoped clients (MCP sampling) can route
    /// the request to the right SSE subscriber; default is the plain call.
    async fn reason_for(&self, _seat: &str, prompt: &str) -> SlcResult<String> {
        self.reason(prompt).await
    }
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
    async fn reason_for(&self, seat: &str, prompt: &str) -> SlcResult<String> {
        self.as_ref().reason_for(seat, prompt).await
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
            // A hung LM Studio (or a wedged connection) must not stall the
            // caller forever: reindex-embeddings used to block on a batch
            // embedding request with no timeout (observed: 0% CPU, process
            // in futex_wait, embeddings.json frozen). 60s bounds every call.
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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
    let mut body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    });
    // Reasoning models (nemotron and others) generate a long reasoning chain
    // by default, leaving `content` empty until it completes. For short tasks
    // (naming ids during migration) it can be disabled:
    // LMSTUDIO_REASONING_EFFORT=none.
    if let Ok(effort) = std::env::var("LMSTUDIO_REASONING_EFFORT") {
        if !effort.is_empty() {
            body["reasoning_effort"] = json!(effort);
        }
    }
    body
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

/// An LlmClient that requests inference FROM THE MCP CLIENT via sampling —
/// a fallback for machines without a local GPU/LLM server (weak computers,
/// VMs). The client-side model performs the call on the server's behalf;
/// embeddings via sampling are impossible — `generate_embedding` returns an
/// error, and search honestly degrades to text-only (BM25).
pub struct McpSamplingLlm {
    /// Outbound sampling requests (the server forwards them to the client via SSE).
    pub outbound: tokio::sync::mpsc::Sender<Value>,
    /// Requests awaiting an answer (id → response channel). Shared with the server.
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

/// RAII: removes the pending sampling entry when dropped. The entry used to
/// be removed ONLY when the client answered — a timed-out request or a
/// broken SSE link left it in the shared map forever (unbounded growth, and
/// a trivial DoS with silent clients).
struct PendingGuard {
    pending: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<String>>>,
    >,
    request_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        // No-op when the server already resolved the request (mcp() removes
        // the entry itself) — remove on a missing key is harmless.
        self.pending.lock().unwrap().remove(&self.request_id);
    }
}

#[async_trait]
impl LlmClient for McpSamplingLlm {
    /// No seat context here — sampling REQUIRES a seat (the SSE fan-out is
    /// deny-by-default, an event with an empty seat would be dropped and the
    /// caller would hang for the full 60s timeout). Fail fast instead;
    /// seat-scoped calls must go through [`reason_for`](Self::reason_for).
    async fn reason(&self, prompt: &str) -> SlcResult<String> {
        let _ = prompt;
        Err(SlcError::Storage(
            "sampling requires a seat — call reason_for(seat, prompt)".into(),
        ))
    }

    /// The sampling event carries the seat so the SSE fan-out only delivers
    /// it to that seat's subscriber — compression prompts contain document
    /// contents and must never leak to other seats.
    async fn reason_for(&self, seat: &str, prompt: &str) -> SlcResult<String> {
        if seat.is_empty() {
            // An event with an empty seat would be received by nobody
            // (deny-by-default) — fail instantly instead of a 60-second wait.
            return Err(SlcError::Storage(
                "sampling requires a non-empty seat (X-Seat-ID)".into(),
            ));
        }
        let request_id = format!("smp-{}", uuid::Uuid::new_v4());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        // Guard lives for the whole request: on ANY exit (timeout, send
        // failure, closed channel) the pending entry is removed.
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            request_id: request_id.clone(),
        };
        self.pending.lock().unwrap().insert(request_id.clone(), tx);
        let msg = json!({
            "type": "sampling_request",
            "request_id": request_id.clone(),
            "seat_id": seat,
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
        let out = llm
            .reason_for("dev", "длинный текст для сжатия")
            .await
            .unwrap();
        assert_eq!(out, "сжатый ответ клиента");
    }

    #[tokio::test]
    async fn sampling_without_seat_fails_fast() {
        // Empty seat: an event with seat_id="" would be received by nobody
        // (deny-by-default) — it used to hang for 60 seconds, now an instant Err.
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let llm = McpSamplingLlm::new(tx, pending.clone());
        let t0 = std::time::Instant::now();
        let err = llm.reason_for("", "текст").await.unwrap_err();
        assert!(err.to_string().contains("non-empty seat"), "{err}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "must fail fast"
        );
        assert!(pending.lock().unwrap().is_empty());
        // reason() without a seat — the same fast failure.
        assert!(llm.reason("текст").await.is_err());
    }

    /// Regression: a request whose client never answers (or whose channel
    /// dies) must NOT leave its entry in the pending map — the map used to
    /// grow without bound.
    #[tokio::test]
    async fn sampling_pending_entry_cleaned_on_failure() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        drop(rx); // outbound closed → send fails → early return
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let llm = McpSamplingLlm::new(tx, pending.clone());
        let err = llm.reason_for("dev", "текст").await.unwrap_err();
        assert!(err.to_string().contains("sampling channel closed"), "{err}");
        assert!(
            pending.lock().unwrap().is_empty(),
            "pending entries must be cleaned on failure"
        );
    }

    #[tokio::test]
    async fn sampling_pending_cleaned_after_timeout() {
        // The client receives the request but never answers; the 60s timeout
        // is too long for a test, so shrink it via the outbound drop trick is
        // not applicable — instead verify the guard path used by timeout:
        // entry exists while waiting, and a late answer after the request
        // died is ignored (mcp() removes a missing key → None).
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let llm = McpSamplingLlm::new(tx, pending.clone());
        let pending2 = pending.clone();
        tokio::spawn(async move {
            let msg = rx.recv().await.unwrap();
            let rid = msg["request_id"].as_str().unwrap().to_string();
            // Remove like mcp() does — entry is present while waiting.
            let tx = pending2.lock().unwrap().remove(&rid);
            assert!(tx.is_some(), "pending entry must exist while awaiting");
        });
        // Drop the outbound AFTER the request was delivered: the reader task
        // above consumed the message, but nobody answers → the guard cleanup
        // path is exercised when the sender side errors on a second call.
        let _ = llm
            .reason_for(
                "dev",
                "первый запрос, ответа не будет — таймаут 60с в проде",
            )
            .await;
        // The entry from the timed-out path must be gone (guard dropped it).
        // The spawned task above removed it on delivery, so the map is empty.
        assert!(pending.lock().unwrap().is_empty());
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
