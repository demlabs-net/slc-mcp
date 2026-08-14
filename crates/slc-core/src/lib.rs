//! slc-core — SLC (Smart Layered Context) agent memory engine.
//!
//! User-approved design rules (see `.agents/skills/slc-mcp/SKILL.md`):
//! - ONE unified `Document` entity (projects/tasks/docs); ids are unique
//!   human-readable NAMES, `content_hash` only for dedup.
//! - Episodic history lives in a SEPARATE store (`episodic_*`), never
//!   embedded, never searched — not part of the RAG store.
//! - DEFAULT backend: Obsidian vault (folders + human-readable file names);
//!   SQLite is the alternative embedded backend.
//! - Ships as `rlib` (embedded in Rust apps, e.g. vs-memory) + `staticlib`
//!   (C ABI) + consumed by the standalone `slc-mcp` binary (MCP server/CLI).

#[cfg(feature = "candle-emb")]
pub mod candle_emb;

pub mod auth;
pub mod error;
pub mod focus;
pub mod llm;
pub mod memory;
pub mod migrate;
pub mod model;
pub mod notifications;
pub mod pagination;
pub mod proactivity;
pub mod profiles;
pub mod reminders;
pub mod seed;
pub mod search;
pub mod seat;
pub mod storage;
pub mod tasks;
pub mod timer;

pub use auth::{AuthMode, Principal, auth_mode_from_env, authenticate};
pub use error::{SlcError, SlcResult};
pub use focus::{FocusItem, FocusManager};
pub use llm::{LlmClient, LmStudioClient, McpSamplingLlm, MockLlm, OllamaClient};
pub use memory::{CompressionReport, ConsolidationReport, HistoryCompressor, MemoryConsolidator};
pub use model::*;
pub use notifications::NotificationQueue;
pub use pagination::Paginator;
pub use proactivity::{MindType, mind_matches, normalize_write_mind_type};
pub use profiles::ProfileManager;
pub use reminders::{ReminderManager, parse_remind_at};
pub use search::{RankWeights, SearchHit, SearchService};
pub use seat::SeatManager;
pub use storage::{DocFilter, DocSort, MetaPatch, SortDir, SortField, StorageBackend};
pub use tasks::{ProjectInfo, TaskInfo, WorkItemManager};
pub use timer::{TimerHandler, TimerRegistry};

/// Which storage backend to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    /// Obsidian vault — DEFAULT (markdown + frontmatter, folder-organized).
    ObsidianVault,
    /// Embedded SQLite file.
    Sqlite,
    /// MongoDB (self-hosted or Atlas) — shared-server option.
    MongoDB,
}

/// Пересобрать эмбеддинги документов БАТЧЕМ: один forward на чанк
/// (по 32 документа), вместо последовательных вызовов. Best-effort —
/// ошибка модели (например, ещё грузится) пропускает чанк целиком,
/// следующий запуск повторит. Возвращает число успешно пересобранных.
pub async fn reembed_documents(
    store: &dyn StorageBackend,
    llm: &dyn LlmClient,
    docs: &[model::Document],
) -> usize {
    const CHUNK: usize = 32;
    let mut done = 0usize;
    for chunk in docs.chunks(CHUNK) {
        let texts: Vec<String> = chunk.iter().map(|d| d.content.clone()).collect();
        let embs = match llm
            .generate_embeddings(&texts, crate::llm::EmbeddingKind::Passage)
            .await
        {
            Ok(embs) => embs,
            Err(e) => {
                tracing::debug!("reembed: chunk skipped: {e}");
                continue; // модель не готова — следующий запуск повторит
            }
        };
        for (doc, emb) in chunk.iter().zip(embs) {
            let (scope, seat_id) = match &doc.seat_id {
                Some(s) => (model::EmbeddingScope::Private, Some(s.clone())),
                None => (model::EmbeddingScope::Public, None),
            };
            let rec = model::EmbeddingRecord {
                document_id: doc.document_id.clone(),
                chunk_index: 0,
                chunk_total: 1,
                embedding: emb.clone(),
                embedding_model: llm.embedding_model_name(),
                embedding_dimension: emb.len(),
                generated_at: chrono::Utc::now(),
                scope,
                seat_id,
            };
            if store.delete_embeddings(&doc.document_id).await.is_ok()
                && store.insert_embeddings(&[rec]).await.is_ok()
            {
                done += 1;
            }
        }
    }
    done
}


/// Best-effort seeding of core documents (manifest, standards, …) on open.
async fn seed_core_if_missing(store: &dyn StorageBackend) {
    match seed::ensure_core_documents(store).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(inserted = n, "seeded core documents"),
        Err(e) => tracing::warn!("core seeding skipped: {e}"),
    }
}

/// Приблизительное число символов на один токен (смешанный RU/EN текст).
/// Компрессия работает в символах, лимиты задаются в токенах.
pub const CHARS_PER_TOKEN: usize = 3;

/// Отчёт пересборки эмбеддингов (`reindex-embeddings`). (`reindex-embeddings`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReindexReport {
    /// Документов всего (в выборке).
    pub total: usize,
    /// Пересобрано эмбеддингов (best-effort).
    pub reindexed: usize,
    /// Пропущено (другой сид при --seat).
    pub skipped: usize,
}

/// Engine configuration.
#[derive(Debug, Clone)]
pub struct SlcConfig {
    pub storage: StorageKind,
    /// Vault dir (Obsidian) or db file (SQLite); `:memory:` allowed.
    pub path: String,
    pub auto_git_commit: bool,
    pub ollama_endpoint: String,
    pub ollama_reasoning_model: String,
    pub ollama_embedding_model: String,
    /// LM Studio (OpenAI-compatible) — preferred when set.
    pub lmstudio_url: Option<String>,
    /// Agentic/reasoning model (compression, consolidation, subagents).
    pub lmstudio_model: String,
    pub lmstudio_embed_model: String,
    pub semantic_weight: f32,
    pub text_weight: f32,
    pub seat_ttl_seconds: i64,
    /// Total context budget in TOKENS handed to the model per
    /// `update_context` call. Per-seat override: `/limit N` (tokens) or the
    /// client's window from `initialize` (tokens). The compression works in
    /// characters internally: budget_chars = tokens * CHARS_PER_TOKEN.
    pub context_limit_tokens: usize,
    /// MongoDB connection URI (used when `storage = MongoDB`).
    pub mongodb_uri: Option<String>,
    /// Use the MCP client's own inference (sampling) as the LLM — the
    /// fallback for weak machines without a local GPU/LLM server.
    pub mcp_sampling: bool,
    /// AI document placement: on `add_document` the reasoning LLM decides
    /// which existing project the document belongs to (folder resolved from
    /// `metadata.extra["project"]`). Best-effort — hash/candle providers
    /// without reasoning simply keep the default folder. Disable with
    /// `SLC_AI_ORGANIZE=false`.
    pub ai_organize: bool,
}

impl Default for SlcConfig {
    fn default() -> Self {
        SlcConfig {
            storage: StorageKind::ObsidianVault, // DEFAULT = Obsidian vault
            path: std::env::var("SLC_VAULT_PATH").unwrap_or_else(|_| "~/.slc/vault".into()),
            auto_git_commit: std::env::var("OBSIDIAN_AUTO_GIT_COMMIT").is_ok_and(|v| v == "true"),
            ollama_endpoint: std::env::var("OLLAMA_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:11434".into()),
            ollama_reasoning_model: std::env::var("OLLAMA_REASONING_MODEL")
                .unwrap_or_else(|_| "gemma3:latest".into()),
            ollama_embedding_model: std::env::var("OLLAMA_EMBEDDING_MODEL")
                .unwrap_or_else(|_| "bge-m3".into()),
            lmstudio_url: std::env::var("LMSTUDIO_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            lmstudio_model: std::env::var("LMSTUDIO_MODEL")
                .unwrap_or_else(|_| "google/gemma-4-e4b".into()),
            lmstudio_embed_model: std::env::var("LMSTUDIO_EMBED_MODEL")
                .unwrap_or_else(|_| "text-embedding-nomic-embed-text-v1.5".into()),
            semantic_weight: 0.7,
            text_weight: 0.3,
            seat_ttl_seconds: 86400,
            // Токены. Легаси- env в символах конвертируем (≈3 симв/токен).
            context_limit_tokens: std::env::var("SLC_CONTEXT_LIMIT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .or_else(|| {
                    std::env::var("SLC_CONTEXT_LIMIT_CHARS")
                        .ok()
                        .and_then(|v| v.parse::<usize>().ok())
                        .map(|chars| chars / CHARS_PER_TOKEN)
                })
                .unwrap_or(100_000),
            mongodb_uri: std::env::var("SLC_MONGODB_URI").ok(),
            mcp_sampling: std::env::var("SLC_MCP_SAMPLING").is_ok_and(|v| v == "true" || v == "1"),
            ai_organize: std::env::var("SLC_AI_ORGANIZE")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
        }
    }
}

/// AI document placement: ask the reasoning LLM which existing project the
/// document belongs to; on a match, bind it via `metadata.extra["project"]`
/// (the folder is resolved from that by [`model::Document::default_folder`]).
/// Best-effort by design — any failure leaves the document at its default
/// folder (hash/candle providers without reasoning simply never match).
async fn organize_document(
    llm: &dyn LlmClient,
    store: &dyn StorageBackend,
    doc: &mut model::Document,
) {
    if doc.category == model::DocumentCategory::History
        || doc.category == model::DocumentCategory::Project
    {
        return;
    }
    let Ok(projects) = store
        .kb_find(
            &DocFilter {
                category: Some(model::DocumentCategory::Project),
                ..Default::default()
            },
            &DocSort::by_created(SortDir::Asc),
            200,
        )
        .await
    else {
        return;
    };
    if projects.is_empty() {
        return;
    }
    let names: Vec<String> = projects
        .iter()
        .map(|p| {
            let name = p
                .metadata
                .extra
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&p.document_id);
            format!("{}: {}", p.document_id, name)
        })
        .collect();
    let snippet: String = doc.content.chars().take(800).collect();
    let prompt = format!(
        "Ты — организатор документов памяти SLC. Определи, к какому проекту относится документ.\n\
         Существующие проекты (id: имя):\n{}\n\
         Категория документа: {}\n\
         Если документ явно относится к одному из проектов — ответь ТОЛЬКО его id.\n\
         Иначе ответь: none\n\n\
         Документ:\n{snippet}",
        names.join("\n"),
        doc.category.as_str(),
    );
    let Ok(answer) = llm.reason(&prompt).await else { return };
    let answer = answer.trim().to_lowercase();
    if answer.is_empty() || answer == "none" {
        return;
    }
    let matched = projects.iter().find(|p| p.document_id == answer).or_else(|| {
        projects.iter().find(|p| {
            p.metadata
                .extra
                .get("name")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.to_lowercase() == answer)
        })
    });
    if let Some(p) = matched {
        doc.metadata
            .extra
            .insert("project".into(), serde_json::json!(p.document_id));
    }
}

/// High-level engine facade — what the app (vs-memory) and the MCP binary use.
pub struct SlcEngine {
    store: std::sync::Arc<dyn StorageBackend>,
    llm: std::sync::Arc<dyn LlmClient>,
    pub seats: SeatManager<std::sync::Arc<dyn StorageBackend>>,
    search: SearchService,
    compressor:
        HistoryCompressor<std::sync::Arc<dyn StorageBackend>, std::sync::Arc<dyn LlmClient>>,
    consolidator:
        MemoryConsolidator<std::sync::Arc<dyn StorageBackend>, std::sync::Arc<dyn LlmClient>>,
    pub config: SlcConfig,
    scheduler: std::sync::Mutex<Option<timer::TimerRegistry>>,
}
impl SlcEngine {
    /// Synchronous open for the embedded backends (Obsidian vault, SQLite).
    /// MongoDB needs an async connect — use [`Self::open_async`].
    pub fn open(config: SlcConfig) -> SlcResult<Self> {
        if config.storage == StorageKind::MongoDB {
            return Err(SlcError::Storage(
                "MongoDB storage requires the async `SlcEngine::open_async`".into(),
            ));
        }
        let store: std::sync::Arc<dyn StorageBackend> = match config.storage {
            StorageKind::ObsidianVault => {
                let path = expand_tilde(&config.path);
                std::sync::Arc::new(storage::obsidian::ObsidianVaultStore::open(
                    path,
                    config.auto_git_commit,
                )?)
            }
            StorageKind::Sqlite => {
                let path = expand_tilde(&config.path);
                std::sync::Arc::new(storage::sqlite::SqliteStore::open(path)?)
            }
            StorageKind::MongoDB => unreachable!(),
        };
        Ok(Self::with(store, Self::pick_llm(), config))
    }

    /// Async open — required for the MongoDB backend, fine for the others.
    pub async fn open_async(config: SlcConfig) -> SlcResult<Self> {
        let store = Self::open_store(&config).await?;
        seed_core_if_missing(store.as_ref()).await;
        Ok(Self::with(store, Self::pick_llm(), config))
    }

    /// Open with an externally-built LLM (e.g. MCP-sampling fallback).
    pub async fn open_async_with_llm(
        config: SlcConfig,
        llm: std::sync::Arc<dyn LlmClient>,
    ) -> SlcResult<Self> {
        let store = Self::open_store(&config).await?;
        seed_core_if_missing(store.as_ref()).await;
        Ok(Self::with(store, llm, config))
    }

    /// Open just the storage backend for the config.
    pub async fn open_store(config: &SlcConfig) -> SlcResult<std::sync::Arc<dyn StorageBackend>> {
        Ok(match config.storage {
            StorageKind::ObsidianVault => {
                let path = expand_tilde(&config.path);
                std::sync::Arc::new(storage::obsidian::ObsidianVaultStore::open(
                    path,
                    config.auto_git_commit,
                )?)
            }
            StorageKind::Sqlite => {
                let path = expand_tilde(&config.path);
                std::sync::Arc::new(storage::sqlite::SqliteStore::open(path)?)
            }
            StorageKind::MongoDB => std::sync::Arc::new(
                storage::mongodb::MongoStore::connect(config.mongodb_uri.as_deref()).await?,
            ),
        })
    }

    /// Provider selection (user-approved cascade):
    /// 1. Explicit `SLC_LLM` wins: `hash` | `ollama` | `lmstudio` | `candle`.
    /// 2. Otherwise legacy envs: `LMSTUDIO_URL` → LM Studio, `OLLAMA_ENDPOINT`
    ///    → Ollama — an explicit provider stays the provider even when it
    ///    fails (search degrades to text-only, no silent switching).
    /// 3. Nothing configured → onboard: GPU + downloaded model → candle
    ///    embeddings (bge-m3); otherwise → CPU-hash embeddings. The model is
    ///    NEVER auto-downloaded — `slc-mcp init` prepares it.
    fn pick_llm() -> std::sync::Arc<dyn LlmClient> {
        match std::env::var("SLC_LLM").as_deref() {
            Ok("hash") => return std::sync::Arc::new(llm::CpuHashLlm),
            Ok("ollama") => return std::sync::Arc::new(OllamaClient::from_env()),
            Ok("lmstudio") => {
                if let Some(c) = LmStudioClient::from_env() {
                    return std::sync::Arc::new(c);
                }
            }
            Ok("candle") => {
                #[cfg(feature = "candle-emb")]
                return std::sync::Arc::new(candle_emb::CandleEmbeddingLlm::new());
            }
            _ => {}
        }
        if let Some(c) = LmStudioClient::from_env() {
            return std::sync::Arc::new(c);
        }
        if std::env::var("OLLAMA_ENDPOINT").is_ok() {
            return std::sync::Arc::new(OllamaClient::from_env());
        }
        #[cfg(feature = "candle-emb")]
        {
            if !candle_emb::CandleEmbeddingLlm::gpu_available() {
                tracing::warn!(
                    "no GPU — CPU-hash embeddings by default; force local CPU inference with SLC_LLM=candle"
                );
                return std::sync::Arc::new(llm::CpuHashLlm);
            }
            let onboard = candle_emb::CandleEmbeddingLlm::new();
            if onboard.model_cached() {
                tracing::info!("GPU detected — onboard candle embeddings (bge-m3)");
                return std::sync::Arc::new(onboard);
            }
            tracing::warn!(
                "GPU detected, but the embedding model is not downloaded — run `slc-mcp init`; CPU-hash embeddings for now"
            );
            std::sync::Arc::new(llm::CpuHashLlm)
        }
        #[cfg(not(feature = "candle-emb"))]
        {
            std::sync::Arc::new(llm::CpuHashLlm)
        }
    }

    /// Build from already-constructed parts (tests, embedding into vs-memory
    /// with a custom LlmClient).
    pub fn with(
        store: std::sync::Arc<dyn StorageBackend>,
        llm: std::sync::Arc<dyn LlmClient>,
        config: SlcConfig,
    ) -> Self {
        let seats = SeatManager::new(store.clone(), config.seat_ttl_seconds);
        let search = SearchService::new(
            store.clone(),
            llm.clone(),
            config.semantic_weight,
            config.text_weight,
        );
        let compressor = HistoryCompressor::new(store.clone(), llm.clone());
        let consolidator = MemoryConsolidator::new(store.clone(), llm.clone());
        SlcEngine {
            store,
            llm,
            seats,
            search,
            compressor,
            consolidator,
            config,
            scheduler: std::sync::Mutex::new(None),
        }
    }

    pub fn store(&self) -> &dyn StorageBackend {
        self.store.as_ref()
    }

    /// Intelligent compression of a text via the reasoning LLM: keep the key
    /// facts within `budget_chars`. Best-effort — on LLM failure the original
    /// text is returned unchanged (callers never truncate by hand).
    pub async fn summarize_text(&self, text: &str, budget_chars: usize) -> SlcResult<String> {
        let prompt = format!(
            "Сожми следующий текст до ключевых фактов (не более {budget_chars} символов).              Только факты, без воды, сохрани имена и цифры:\n\n{text}"
        );
        let out = self.llm.reason(&prompt).await?.trim().to_string();
        if out.is_empty() {
            return Ok(text.to_string());
        }
        Ok(out)
    }

    /// Effective context budget for a seat (TOKENS): per-seat override
    /// (`context_limit_tokens` from /limit or initialize) wins over the
    /// config default. Legacy `context_limit_chars` values are converted.
    pub async fn context_limit_for(&self, seat_id: &str) -> SlcResult<usize> {
        if let Ok(Some(seat)) = self.seats.get_seat(seat_id).await {
            if let Some(v) = seat
                .context
                .get("context_limit_tokens")
                .and_then(|v| v.as_u64())
            {
                return Ok(v as usize);
            }
            if let Some(v) = seat
                .context
                .get("context_limit_chars")
                .and_then(|v| v.as_u64())
            {
                return Ok((v as usize / CHARS_PER_TOKEN).max(1));
            }
        }
        Ok(self.config.context_limit_tokens)
    }

    /// The active LLM provider (LM Studio or Ollama).
    pub fn llm(&self) -> &dyn LlmClient {
        self.llm.as_ref()
    }

    // ── knowledge base (RAG-eligible) ────────────────────────────

    /// Add a KB document. With `config.ai_organize` and no explicit
    /// `folder`, the reasoning LLM first decides which existing project the
    /// document belongs to — the folder is then resolved hierarchically
    /// (`docs/projects/<p>/<category>/` or `docs/<category>/`). Best-effort:
    /// any LLM failure/timeout keeps the default folder.
    pub async fn add_document(&self, doc: &mut Document) -> SlcResult<()> {
        if self.config.ai_organize && doc.folder.is_none() {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(25),
                organize_document(&*self.llm, self.store.as_ref(), doc),
            )
            .await;
        }
        self.store.kb_insert(doc).await?;
        // Best-effort embedding so semantic search covers KB documents (never
        // fails the insert; the model may still be downloading).
        self.embed_document(doc).await;
        Ok(())
    }

    /// Embed one document for semantic search (chunk_total = 1; long docs
    /// are truncated by the model's max length). Best-effort by design.
    async fn embed_document(&self, doc: &Document) {
        let Ok(emb) = self
            .llm
            .generate_embedding_kind(&doc.content, crate::llm::EmbeddingKind::Passage)
            .await
        else {
            return;
        };
        let (scope, seat_id) = match &doc.seat_id {
            Some(s) => (model::EmbeddingScope::Private, Some(s.clone())),
            None => (model::EmbeddingScope::Public, None),
        };
        let rec = model::EmbeddingRecord {
            document_id: doc.document_id.clone(),
            chunk_index: 0,
            chunk_total: 1,
            embedding: emb.clone(),
            embedding_model: self.llm.embedding_model_name(),
            embedding_dimension: emb.len(),
            generated_at: chrono::Utc::now(),
            scope,
            seat_id,
        };
        let _ = self.store.delete_embeddings(&doc.document_id).await;
        let _ = self.store.insert_embeddings(&[rec]).await;
    }

    /// Пересобрать эмбеддинги всех документов (или одного сита) текущим
    /// embedding-провайдером — после смены модели/настроек (`slc-mcp
    /// reindex-embeddings`). Старые записи с другой размерностью всё равно
    /// отфильтровываются поиском, но пересборка возвращает семантику.
    pub async fn reindex_embeddings(&self, seat: Option<&str>) -> SlcResult<ReindexReport> {
        let docs = self
            .store
            .kb_find(
                &storage::DocFilter::default(),
                &storage::DocSort::by_updated(storage::SortDir::Desc),
                usize::MAX,
            )
            .await?;
        let selected: Vec<model::Document> = docs
            .iter()
            .filter(|d| {
                seat.map(|s| d.seat_id.as_deref() == Some(s))
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        let skipped = docs.len() - selected.len();
        // Батч-пересборка (один forward на чанк документов).
        let done = reembed_documents(self.store.as_ref(), self.llm.as_ref(), &selected).await;
        Ok(ReindexReport {
            total: docs.len(),
            reindexed: done,
            skipped,
        })
    }

    pub async fn get_document(&self, document_id: &str) -> SlcResult<Option<Document>> {
        self.store.kb_get(document_id).await
    }

    pub async fn search(
        &self,
        query: &str,
        seat_id: Option<&str>,
        limit: usize,
    ) -> SlcResult<Vec<SearchHit>> {
        self.search.search(query, None, None, limit, seat_id).await
    }

    // ── episodic memory ──────────────────────────────────────────

    /// Record a raw event (L1) into the episodic store — the diary entry.
    pub async fn remember(&self, seat_id: &str, event_id: &str, content: &str) -> SlcResult<()> {
        let mut meta = model::DocMeta::default();
        meta.doc_type = Some("EPISODIC".into());
        meta.doc_level = Some(model::DocLevel::L1);
        meta.seat_id = Some(seat_id.into());
        let doc = model::Document::new(
            event_id,
            model::DocumentCategory::History,
            content,
            meta,
            vec!["episodic".into()],
            Some(seat_id.into()),
        );
        self.store.episodic_insert(&doc).await
    }

    /// Run the L1→L4 compression for a seat (triggered by HISTORY_COMPRESSION).
    pub async fn compress(&self, seat_id: &str) -> SlcResult<CompressionReport> {
        self.compressor.compress(seat_id).await
    }

    /// Extract learned facts (triggered by CONSOLIDATION).
    pub async fn consolidate(&self, seat_id: &str) -> SlcResult<ConsolidationReport> {
        self.consolidator.consolidate(seat_id).await
    }

    /// Episodic recall — search HISTORY only (separate from KB search).
    pub async fn recall(&self, seat_id: &str, limit: usize) -> SlcResult<Vec<Document>> {
        let f = DocFilter {
            seat_id: Some(seat_id.into()),
            ..Default::default()
        };
        self.store
            .episodic_find(&f, &DocSort::by_created(SortDir::Desc), limit)
            .await
    }

    // ── proactive loop: focuses ────────────────────────────────────

    /// Add a focus; returns the created item.
    pub async fn focus_add(
        &self,
        seat_id: &str,
        title: &str,
        description: &str,
        priority: i64,
        depends_on: &[String],
        mind_type: Option<&str>,
    ) -> SlcResult<FocusItem> {
        let fm = FocusManager::new(self.store.clone());
        fm.add(seat_id, title, description, priority, depends_on, mind_type)
            .await
    }

    pub async fn focus_remove(&self, seat_id: &str, focus_id: &str) -> SlcResult<bool> {
        let fm = FocusManager::new(self.store.clone());
        fm.remove(focus_id, Some(seat_id)).await
    }

    pub async fn focus_update(
        &self,
        seat_id: &str,
        focus_id: &str,
        title: Option<&str>,
        description: Option<&str>,
        priority: Option<i64>,
        depends_on: Option<&[String]>,
    ) -> SlcResult<bool> {
        let fm = FocusManager::new(self.store.clone());
        fm.update(
            focus_id,
            title,
            description,
            priority,
            depends_on,
            Some(seat_id),
        )
        .await
    }

    pub async fn focus_list(
        &self,
        seat_id: &str,
        mind_type: Option<MindType>,
    ) -> SlcResult<Vec<FocusItem>> {
        let fm = FocusManager::new(self.store.clone());
        fm.get_active(seat_id, mind_type).await
    }

    // ── reminders + notifications ────────────────────────────────

    /// Create a reminder; schedules a one-shot `REMINDER` timer.
    pub async fn reminder_create(
        &self,
        seat_id: &str,
        content: &str,
        remind_at: chrono::DateTime<chrono::Utc>,
        mind_type: Option<&str>,
    ) -> SlcResult<model::Reminder> {
        let registry = self
            .scheduler()
            .unwrap_or_else(|| timer::TimerRegistry::new(self.store.clone()));
        let manager = ReminderManager::new(self.store.clone(), registry);
        manager
            .create(seat_id, content, remind_at, None, None, false, mind_type)
            .await
    }

    pub async fn reminder_list(&self, seat_id: &str) -> SlcResult<Vec<model::Reminder>> {
        let registry = timer::TimerRegistry::new(self.store.clone());
        let manager = ReminderManager::new(self.store.clone(), registry);
        manager.list(seat_id, None, None).await
    }

    pub async fn reminder_cancel(&self, seat_id: &str, reminder_id: &str) -> SlcResult<bool> {
        let registry = self
            .scheduler()
            .unwrap_or_else(|| timer::TimerRegistry::new(self.store.clone()));
        let manager = ReminderManager::new(self.store.clone(), registry);
        manager.cancel(reminder_id, Some(seat_id)).await
    }

    /// Pop pending notifications for the seat (marks them delivered).
    pub async fn pop_notifications(
        &self,
        seat_id: &str,
        limit: usize,
    ) -> SlcResult<Vec<model::Notification>> {
        let queue = NotificationQueue::new(self.store.clone());
        queue.pop_pending(seat_id, limit).await
    }

    pub async fn pending_notification_count(&self, seat_id: &str) -> SlcResult<usize> {
        let queue = NotificationQueue::new(self.store.clone());
        queue.count_pending(seat_id).await
    }

    // ── pagination ───────────────────────────────────────────────

    pub async fn paginate(
        &self,
        seat_id: &str,
        response_id: &str,
        data: &serde_json::Value,
    ) -> SlcResult<serde_json::Value> {
        let p = Paginator::new(self.store.clone());
        p.paginate(seat_id, response_id, data).await
    }

    pub async fn get_page(
        &self,
        seat_id: &str,
        response_id: &str,
        page: usize,
    ) -> SlcResult<serde_json::Value> {
        let p = Paginator::new(self.store.clone());
        p.get_page(seat_id, response_id, page).await
    }

    pub async fn delete_response(&self, response_id: &str) -> SlcResult<serde_json::Value> {
        let p = Paginator::new(self.store.clone());
        p.delete_response(response_id).await
    }

    pub async fn set_page_limit(&self, tokens: usize) -> SlcResult<serde_json::Value> {
        let p = Paginator::new(self.store.clone());
        p.set_page_limit(tokens).await
    }

    pub async fn page_settings(&self) -> SlcResult<serde_json::Value> {
        let p = Paginator::new(self.store.clone());
        p.get_settings().await
    }

    // ── tasks + projects (unified Documents) ─────────────────────

    pub async fn task_create(
        &self,
        seat_id: &str,
        name: &str,
        description: &str,
        project_id: Option<&str>,
        auto_load: &[String],
        metadata: &serde_json::Value,
    ) -> SlcResult<tasks::TaskInfo> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.create_task(seat_id, name, description, project_id, auto_load, metadata)
            .await
    }

    pub async fn task_get(
        &self,
        seat_id: &str,
        task_id: &str,
    ) -> SlcResult<Option<tasks::TaskInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.get_task(seat_id, task_id).await
    }

    pub async fn task_update(
        &self,
        seat_id: &str,
        task_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        project_id: Option<Option<&str>>,
        auto_load: Option<&[String]>,
        status: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> SlcResult<Option<tasks::TaskInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.update_task(
            seat_id,
            task_id,
            name,
            description,
            project_id,
            auto_load,
            status,
            metadata,
        )
        .await
    }

    pub async fn task_delete(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.delete_task(seat_id, task_id).await
    }

    pub async fn task_list(
        &self,
        seat_id: &str,
        project_id: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> SlcResult<Vec<tasks::TaskInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.list_tasks(seat_id, project_id, status, limit).await
    }

    pub async fn task_activate(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.set_active_task(seat_id, task_id).await
    }

    // ── active document (unified activation — any category) ────────────

    /// Activate ANY document (task, project, skill, knowledge doc…) as the
    /// seat's context anchor: it is included in `update_context` and its
    /// auto_load links are followed. Task activation additionally keeps the
    /// legacy active-task pointer in sync.
    pub async fn document_activate(&self, seat_id: &str, document_id: &str) -> SlcResult<bool> {
        let Some(doc) = self.get_document(document_id).await? else {
            return Ok(false);
        };
        self.store
            .set_seat_active_document(seat_id, Some(document_id))
            .await?;
        if doc.category == DocumentCategory::Task {
            self.store
                .set_seat_active_task(seat_id, document_id)
                .await?;
        }
        Ok(true)
    }

    /// Clear the seat's active document (both unified and task pointers).
    pub async fn document_deactivate(&self, seat_id: &str) -> SlcResult<()> {
        self.seats.set_active_task(seat_id, None, None).await?;
        Ok(())
    }

    /// The seat's active document, if any (task fallback included).
    pub async fn document_get_active(&self, seat_id: &str) -> SlcResult<Option<Document>> {
        let Some(id) = self.store.get_seat_active_document(seat_id).await? else {
            return Ok(None);
        };
        self.get_document(&id).await
    }

    pub async fn task_get_active(&self, seat_id: &str) -> SlcResult<Option<tasks::TaskInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.get_active_task(seat_id).await
    }

    pub async fn project_create(
        &self,
        seat_id: &str,
        name: &str,
        description: &str,
        auto_load: &[String],
        metadata: &serde_json::Value,
    ) -> SlcResult<tasks::ProjectInfo> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.create_project(seat_id, name, description, auto_load, metadata)
            .await
    }

    pub async fn project_get(
        &self,
        seat_id: &str,
        project_id: &str,
    ) -> SlcResult<Option<tasks::ProjectInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.get_project(seat_id, project_id).await
    }

    pub async fn project_update(
        &self,
        seat_id: &str,
        project_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        auto_load: Option<&[String]>,
        status: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> SlcResult<Option<tasks::ProjectInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.update_project(
            seat_id,
            project_id,
            name,
            description,
            auto_load,
            status,
            metadata,
        )
        .await
    }

    pub async fn project_delete(&self, seat_id: &str, project_id: &str) -> SlcResult<bool> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.delete_project(seat_id, project_id).await
    }

    pub async fn project_list(
        &self,
        seat_id: &str,
        status: Option<&str>,
        limit: usize,
    ) -> SlcResult<Vec<tasks::ProjectInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.list_projects(seat_id, status, limit).await
    }

    // ── profiles ─────────────────────────────────────────────────

    pub async fn get_user_profile(&self, seat_id: &str) -> SlcResult<Option<String>> {
        let pm = ProfileManager::new(self.store.clone());
        let user_id = pm.resolve_user_id(seat_id).await?;
        pm.get_user_profile(&user_id, seat_id).await
    }

    pub async fn upsert_user_profile(&self, seat_id: &str, content: &str) -> SlcResult<bool> {
        let pm = ProfileManager::new(self.store.clone());
        let user_id = pm.resolve_user_id(seat_id).await?;
        pm.upsert_user_profile(&user_id, content, seat_id).await
    }

    pub async fn get_seat_profile(&self, seat_id: &str) -> SlcResult<Option<(String, String)>> {
        let pm = ProfileManager::new(self.store.clone());
        pm.get_seat_profile(seat_id).await
    }

    pub async fn upsert_seat_profile(
        &self,
        seat_id: &str,
        content: &str,
        timezone: Option<&str>,
    ) -> SlcResult<bool> {
        let pm = ProfileManager::new(self.store.clone());
        pm.upsert_seat_profile(seat_id, content, timezone).await
    }

    // ── seats ────────────────────────────────────────────────────

    pub async fn ensure_seat(&self, seat_id: &str) -> SlcResult<Seat> {
        self.seats.ensure_seat(seat_id).await
    }

    pub async fn close_seat(&self, seat_id: &str) -> SlcResult<bool> {
        self.seats.close_seat(seat_id).await
    }

    pub async fn cleanup_expired_seats(&self) -> SlcResult<i64> {
        self.seats.cleanup_expired().await
    }

    pub async fn health(&self) -> bool {
        self.store.health_check().await
    }

    /// Start the background timer scheduler: default per-seat timers +
    /// compression/consolidation handlers. Idempotent. The MCP server calls
    /// this on startup.
    pub async fn start_background(&self) -> SlcResult<()> {
        let mut guard = self.scheduler.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        // Housekeeping: expire delivered notifications (24h TTL) and
        // paginated responses (10 min TTL). The cleanup functions existed
        // but NOTHING called them — both queues grew without bound.
        spawn_ttl_cleanup(self.store.clone());
        let registry = timer::TimerRegistry::new(self.store.clone());

        let compressor = self.compressor.clone();
        registry.set_handler(
            model::TimerType::HistoryCompression,
            std::sync::Arc::new(timer::AsyncFnHandler::new(move |t| {
                let compressor = compressor.clone();
                let seat = t.seat_id.clone();
                Box::pin(async move {
                    let report = compressor.compress(&seat).await?;
                    tracing::info!(
                        "history compression [{seat}]: L1→L2 {}, L2→L3 {}, L3→L4 {}",
                        report.l1_to_l2,
                        report.l2_to_l3,
                        report.l3_to_l4
                    );
                    Ok(())
                })
            })),
        );
        let consolidator = self.consolidator.clone();
        registry.set_handler(
            model::TimerType::Consolidation,
            std::sync::Arc::new(timer::AsyncFnHandler::new(move |t| {
                let consolidator = consolidator.clone();
                let seat = t.seat_id.clone();
                Box::pin(async move {
                    let report = consolidator.consolidate(&seat).await?;
                    tracing::info!("consolidation [{seat}]: +{} facts", report.facts_added);
                    Ok(())
                })
            })),
        );
        let store_for_handlers = self.store.clone();
        registry.set_handler(
            model::TimerType::Reminder,
            std::sync::Arc::new(timer::AsyncFnHandler::new(move |t| {
                let store = store_for_handlers.clone();
                let timer = t.clone();
                Box::pin(async move {
                    handle_reminder(&store, &timer).await?;
                    Ok(())
                })
            })),
        );
        let store_for_handlers = self.store.clone();
        registry.set_handler(
            model::TimerType::FocusReminder,
            std::sync::Arc::new(timer::AsyncFnHandler::new(move |t| {
                let store = store_for_handlers.clone();
                let timer = t.clone();
                Box::pin(async move {
                    handle_focus_reminder(&store, &timer).await?;
                    Ok(())
                })
            })),
        );
        // Defaults for every active seat, then start the loops.
        for seat in self.store.list_active_seats(1000).await? {
            let created = registry.create_defaults(&seat.seat_id).await?;
            if !created.is_empty() {
                tracing::debug!(
                    "seat {}: default timers {}",
                    seat.seat_id,
                    created.join(",")
                );
            }
        }
        registry.start().await?;
        *guard = Some(registry);
        Ok(())
    }

    pub fn scheduler(&self) -> Option<timer::TimerRegistry> {
        self.scheduler.lock().unwrap().clone()
    }
}

/// Timer handler for a user-created `REMINDER`: mark fired + push a
/// notification (mirrors `ReminderHandler` in the legacy).
async fn handle_reminder(
    store: &std::sync::Arc<dyn StorageBackend>,
    timer: &model::PersistedTimer,
) -> SlcResult<()> {
    let Some(reminder_id) = timer.metadata.get("reminder_id").and_then(|v| v.as_str()) else {
        tracing::warn!(
            "REMINDER timer {} has no reminder_id in metadata",
            timer.timer_id
        );
        return Ok(());
    };
    let manager = ReminderManager::new(store.clone(), timer::TimerRegistry::new(store.clone()));
    let Some(reminder) = manager.get(reminder_id, Some(&timer.seat_id)).await? else {
        tracing::warn!("Reminder {reminder_id} not found in DB");
        return Ok(());
    };
    manager
        .mark_fired(reminder_id, Some(&timer.seat_id))
        .await?;
    let queue = NotificationQueue::new(store.clone());
    let mut meta = serde_json::Map::new();
    meta.insert(
        "reminder_id".into(),
        serde_json::Value::String(reminder_id.into()),
    );
    queue
        .push(
            &timer.seat_id,
            "REMINDER",
            "⏰ Напоминание",
            &reminder.content,
            meta,
        )
        .await?;
    Ok(())
}

/// Periodic nudge about active focuses (`FocusReminderHandler`).
async fn handle_focus_reminder(
    store: &std::sync::Arc<dyn StorageBackend>,
    timer: &model::PersistedTimer,
) -> SlcResult<()> {
    let focuses = FocusManager::new(store.clone())
        .get_active(&timer.seat_id, None)
        .await?;
    if focuses.is_empty() {
        return Ok(());
    }
    let mut body = format!("У вас {} активных фокусов:\n", focuses.len());
    for f in focuses.iter().take(5) {
        body.push_str(&format!("- {} (priority {})\n", f.title, f.priority));
    }
    if focuses.len() > 5 {
        body.push_str(&format!("...и ещё {}", focuses.len() - 5));
    }
    let queue = NotificationQueue::new(store.clone());
    queue
        .push(
            &timer.seat_id,
            "FOCUS_REMINDER",
            "🎯 Текущие фокусы",
            &body,
            Default::default(),
        )
        .await?;
    Ok(())
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

// ───────────────────────────── C ABI (staticlib) ─────────────────────────────
// Minimal FFI surface for embedding into non-Rust hosts: get/remember/search.
// Full access is via the Rust `rlib` API; these functions are conveniences.

use std::ffi::{CStr, CString};

/// Remember an episodic event on the engine. Returns 0 on success.
/// # Safety: `engine` must point to a live SlcEngine; `seat`, `event_id`,
/// `content` must be valid NUL-terminated UTF-8 for the call duration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slc_remember(
    engine: *const SlcEngine,
    seat: *const std::os::raw::c_char,
    event_id: *const std::os::raw::c_char,
    content: *const std::os::raw::c_char,
) -> i32 {
    let (Ok(seat), Ok(event_id), Ok(content)) = (
        // SAFETY: caller guarantees NUL-terminated UTF-8 pointers.
        unsafe { CStr::from_ptr(seat) }.to_str(),
        unsafe { CStr::from_ptr(event_id) }.to_str(),
        unsafe { CStr::from_ptr(content) }.to_str(),
    ) else {
        return -1;
    };
    // SAFETY: caller guarantees the engine outlives the call.
    let engine = unsafe { &*engine };
    let handle = tokio::runtime::Handle::try_current();
    let result = match handle {
        Ok(h) => h.block_on(engine.remember(seat, event_id, content)),
        Err(_) => {
            let rt = tokio::runtime::Runtime::new().expect("slc rt");
            rt.block_on(engine.remember(seat, event_id, content))
        }
    };
    result.map(|_| 0).unwrap_or(-2)
}

/// Returns a C string (caller must `slc_free_string`).
#[unsafe(no_mangle)]
pub extern "C" fn slc_version() -> *mut std::os::raw::c_char {
    let v = CString::new(env!("CARGO_PKG_VERSION")).unwrap();
    v.into_raw()
}

/// # Safety: ptr must come from `slc_version`/any slc-allocated CString.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn slc_free_string(ptr: *mut std::os::raw::c_char) {
    if !ptr.is_null() {
        // SAFETY: ptr came from CString::into_raw.
        drop(unsafe { CString::from_raw(ptr) });
    }
}

#[cfg(test)]
mod engine_tests {
    use super::*;

    #[tokio::test]
    async fn summarize_text_uses_llm_and_falls_back() {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        // LLM returns a short summary → used.
        let llm: std::sync::Arc<dyn LlmClient> =
            std::sync::Arc::new(MockLlm::new(vec!["краткий факт".into()]));
        let engine = SlcEngine::with(store.clone(), llm, SlcConfig::default());
        let out = engine
            .summarize_text("очень длинный текст", 100)
            .await
            .unwrap();
        assert_eq!(out, "краткий факт");

        // LLM yields a long echo (no real compression) → the text is never
        // truncated by hand to the budget.
        let llm2: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(MockLlm::new(vec![]));
        let engine2 = SlcEngine::with(store, llm2, SlcConfig::default());
        let out2 = engine2
            .summarize_text("очень длинный текст", 5)
            .await
            .unwrap();
        assert!(out2.len() > 5, "no manual truncation: {out2}");
    }
}

/// Periodically expire old housekeeping records: delivered notifications
/// (24h) and paginated responses (10 min). Runs every 5 minutes; failures
/// are logged, never fatal.
fn spawn_ttl_cleanup(store: std::sync::Arc<dyn crate::storage::StorageBackend>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // Notifications are per-seat.
            match store.list_active_seats(1000).await {
                Ok(seats) => {
                    for seat in seats {
                        let queue = crate::notifications::NotificationQueue::new(store.clone());
                        match queue
                            .cleanup(&seat.seat_id, crate::notifications::TTL_SECONDS)
                            .await
                        {
                            Ok(n) if n > 0 => tracing::info!(
                                seat = %seat.seat_id,
                                removed = n,
                                "expired notifications cleaned"
                            ),
                            Ok(_) => {}
                            Err(e) => tracing::warn!("notification cleanup: {e}"),
                        }
                    }
                }
                Err(e) => tracing::warn!("ttl cleanup: list_active_seats: {e}"),
            }
            // Paginated responses are global.
            let paginator = crate::pagination::Paginator::new(store.clone());
            match paginator.cleanup(crate::pagination::TTL_SECONDS).await {
                Ok(n) if n > 0 => tracing::info!(removed = n, "expired paginated responses cleaned"),
                Ok(_) => {}
                Err(e) => tracing::warn!("pagination cleanup: {e}"),
            }
        }
    });
}
