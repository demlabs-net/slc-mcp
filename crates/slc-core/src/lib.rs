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

use serde_json::json;

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
pub mod roles;
pub mod search;
pub mod seat;
pub mod seed;
pub mod storage;
pub mod tasks;
pub mod timer;
pub mod workflow;

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
pub use workflow::{TaskEvent, TaskEventKind, TaskListScope};

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

/// Rebuild document embeddings IN BATCHES: one forward pass per chunk
/// (of 32 documents) instead of sequential calls. Best-effort — a model
/// error (e.g. still loading) skips the whole chunk, the next run retries.
/// Returns the number of successfully rebuilt documents.
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
                continue; // model not ready — the next run will retry
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

/// Approximate characters per token (mixed RU/EN text).
/// Compression works in characters, limits are set in tokens.
pub const CHARS_PER_TOKEN: usize = 3;

/// Report of the embedding rebuild (`reindex-embeddings`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReindexReport {
    /// Documents in total (in the selection).
    pub total: usize,
    /// Embeddings rebuilt (best-effort).
    pub reindexed: usize,
    /// Skipped (another seat with --seat).
    pub skipped: usize,
}

/// Report of a document rename (`rename_document`/`rename_task`/
/// `rename_project`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RenameReport {
    pub old_id: String,
    pub new_id: String,
    /// Documents affected in total (reference fields and/or content).
    pub links_fixed: usize,
    /// Documents whose content had wiki links [[old]] replaced.
    pub content_links_fixed: usize,
    /// Seats with updated active pointers.
    pub seats_updated: Vec<String>,
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
    /// `update_context` call. A connection header or per-seat `/limit N`
    /// value may override this fallback. The compression counts usage in
    /// tokens too (~3 chars per token via CHARS_PER_TOKEN).
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
    /// Seat roles: seat_id → roles. From env `SLC_SEAT_ROLES`
    /// ("seat_a=operator,seat_b=operator") or programmatically via
    /// [`SlcConfig::with_seat_role`] (staticlib: an initialization parameter).
    pub seat_roles: std::collections::HashMap<String, Vec<roles::SeatRole>>,
    /// Explicit cross-seat scope for operator seats.  From env
    /// `SLC_SEAT_MANAGE_ACL` (`manager=developer|designer,root=*`).
    /// An operator without an ACL entry remains restricted to its own seat.
    pub seat_manage_acl: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// Stable workflow principal → SLC seat mapping. Workflow records never
    /// contain Swarm MCP endpoint names or transport-specific identities.
    pub principal_seats: std::collections::HashMap<String, String>,
    /// Workflow principal → SLC policy document automatically attached to
    /// every new assigned task through its `auto_load` chain.
    pub principal_policy_documents: std::collections::HashMap<String, String>,
    /// Principal-level task delegation ACL, independent from message routing.
    pub task_assign_acl: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// Text-only principals cannot claim visual acceptance in task reports.
    pub text_only_principals: std::collections::HashSet<String>,
}

impl SlcConfig {
    /// Add a role to a seat (builder-style; for embedded staticlib clients).
    pub fn with_seat_role(mut self, seat_id: impl Into<String>, role: roles::SeatRole) -> Self {
        self.seat_roles
            .entry(seat_id.into())
            .or_default()
            .push(role);
        self
    }

    /// Grant an operator access to one explicit target seat.
    pub fn with_seat_manage_target(
        mut self,
        actor: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        self.seat_manage_acl
            .entry(actor.into())
            .or_default()
            .insert(target.into());
        self
    }
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
            // The context budget is in TOKENS (~3 chars/token inside compression).
            context_limit_tokens: std::env::var("SLC_CONTEXT_LIMIT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100_000),
            mongodb_uri: std::env::var("SLC_MONGODB_URI").ok(),
            mcp_sampling: std::env::var("SLC_MCP_SAMPLING").is_ok_and(|v| v == "true" || v == "1"),
            ai_organize: std::env::var("SLC_AI_ORGANIZE")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            seat_roles: roles::parse_roles_env(),
            seat_manage_acl: roles::parse_manage_acl_env(),
            principal_seats: roles::parse_principal_seats_env(),
            principal_policy_documents: roles::parse_principal_policy_documents_env(),
            task_assign_acl: roles::parse_task_assign_acl_env(),
            text_only_principals: roles::parse_text_only_principals_env(),
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
    // The document's seat: sampling inference only works with a seat (otherwise
    // it fails instantly — default folder); local providers ignore it.
    let seat = doc.seat_id.as_deref().unwrap_or("");
    let Ok(answer) = llm.reason_for(seat, &prompt).await else {
        return;
    };
    let answer = answer.trim().to_lowercase();
    if answer.is_empty() || answer == "none" {
        return;
    }
    let matched = projects
        .iter()
        .find(|p| p.document_id == answer)
        .or_else(|| {
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
    /// Serializes workflow projections with their event/idempotency writes.
    /// Storage backends do not expose a shared cross-record transaction API.
    workflow_lock: tokio::sync::Mutex<()>,
    scheduler: tokio::sync::Mutex<Option<timer::TimerRegistry>>,
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
            workflow_lock: tokio::sync::Mutex::new(()),
            scheduler: tokio::sync::Mutex::new(None),
        }
    }

    pub fn store(&self) -> &dyn StorageBackend {
        self.store.as_ref()
    }

    /// Intelligent compression of a text via the reasoning LLM: keep the key
    /// facts within `max_chars` (the size of the compressed text for the LLM
    /// prompt, not the context budget). Seat-aware: sampling inference
    /// requires a seat.
    /// Best-effort — on LLM failure the original text is returned unchanged
    /// (callers never truncate by hand).
    pub async fn summarize_text_for(
        &self,
        seat_id: &str,
        text: &str,
        max_chars: usize,
    ) -> SlcResult<String> {
        let prompt = format!(
            "Сожми следующий текст до ключевых фактов (не более {max_chars} символов).              Только факты, без воды, сохрани имена и цифры:\n\n{text}"
        );
        let out = self
            .llm
            .reason_for(seat_id, &prompt)
            .await?
            .trim()
            .to_string();
        if out.is_empty() {
            return Ok(text.to_string());
        }
        Ok(out)
    }

    /// Seat-less variant for callers without a seat context (returns the
    /// original text when the provider requires a seat, e.g. MCP sampling).
    pub async fn summarize_text(&self, text: &str, max_chars: usize) -> SlcResult<String> {
        match self.summarize_text_for("", text, max_chars).await {
            Ok(s) => Ok(s),
            Err(e) if e.to_string().contains("requires a non-empty seat") => Ok(text.to_string()),
            Err(e) => Err(e),
        }
    }

    /// Effective context budget for a seat (TOKENS): the per-seat
    /// `context_limit_tokens` written by `/limit` wins over the config
    /// default. A connection header is applied by the MCP server above this
    /// layer. Legacy `context_limit_chars` values are converted.
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

    /// The active reasoning provider (including MCP sampling or local hash).
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

    /// Re-embed a document by id (after task/project/document updates that
    /// change the body) so semantic search sees the current content.
    /// Best-effort: returns false when the doc is missing or embedding failed.
    pub async fn reembed_document(&self, document_id: &str) -> bool {
        match self.store.kb_get(document_id).await {
            Ok(Some(doc)) => {
                self.embed_document(&doc).await;
                true
            }
            _ => false,
        }
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

    /// Rebuild embeddings of all documents (or of one seat) with the current
    /// embedding provider — after a model/settings change (`slc-mcp
    /// reindex-embeddings`). Old records with a different dimensionality are
    /// filtered out by search anyway, but the rebuild restores semantics.
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
        // Batch rebuild (one forward per chunk of documents).
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
            .await
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
            .await
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

    /// List notifications for the seat (any status; without popping).
    pub async fn list_notifications(
        &self,
        seat_id: &str,
        status: Option<&str>,
    ) -> SlcResult<Vec<model::Notification>> {
        let queue = NotificationQueue::new(self.store.clone());
        queue.list(seat_id, status).await
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

    pub async fn paginate_with_limit(
        &self,
        seat_id: &str,
        response_id: &str,
        data: &serde_json::Value,
        page_token_limit: usize,
    ) -> SlcResult<serde_json::Value> {
        let p = Paginator::new(self.store.clone());
        p.paginate_with_limit(seat_id, response_id, data, page_token_limit)
            .await
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

    pub async fn page_token_limit(&self) -> SlcResult<usize> {
        let p = Paginator::new(self.store.clone());
        p.page_token_limit().await
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

    /// Update a task with full manager semantics.
    ///
    /// Authorization: the caller may update its own (or public) tasks, or —
    /// as a manager (`operator` role with an explicit `SLC_SEAT_MANAGE_ACL`
    /// target) — tasks of every managed seat. A genuinely unknown id is an
    /// explicit [`SlcError::NotFound`]; an existing task of a non-managed seat
    /// is an explicit [`SlcError::PermissionDenied`] (never a fake
    /// "not found").
    ///
    /// Workflow tasks (see [`crate::tasks::is_workflow_task`]) stay
    /// append-only for their participants and owners — those keep the legacy
    /// `InvalidInput` guidance (use `task_message`/`report_task`/…). Only a
    /// manager of the task owner seat may edit them: the edit is persisted
    /// canonically through the workflow event store
    /// ([`Self::workflow_update_task`]) which appends a durable
    /// `update` audit event and refreshes the projection — never a silent
    /// no-op and never a rewrite behind the event stream.
    pub async fn task_update(
        &self,
        seat_id: &str,
        task_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        description_patch: Option<&serde_json::Value>,
        project_id: Option<Option<&str>>,
        auto_load: Option<&[String]>,
        status: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> SlcResult<tasks::TaskInfo> {
        let Some(doc) = self.get_document(task_id).await? else {
            return Err(SlcError::NotFound(format!("task not found: {task_id}")));
        };
        if doc.category != DocumentCategory::Task {
            return Err(SlcError::NotFound(format!("task not found: {task_id}")));
        }
        let actor_owns = doc.is_kb_visible(seat_id);
        let actor_manages = doc
            .seat_id
            .as_deref()
            .filter(|owner| *owner != seat_id)
            .is_some_and(|owner| self.can_manage_target(seat_id, owner));
        if !actor_owns && !actor_manages {
            return Err(SlcError::PermissionDenied(format!(
                "task {task_id} belongs to another seat; only its owner or a manager (operator role with an SLC_SEAT_MANAGE_ACL target for that seat) may update it"
            )));
        }
        if tasks::is_workflow_task(&doc) {
            // The owner/participant stays on the append-only contract.
            if !actor_manages {
                return Err(SlcError::InvalidInput(
                    "workflow tasks are append-only; use task_message, report_task, or cancel_task"
                        .into(),
                ));
            }
            return self
                .workflow_update_task(
                    seat_id,
                    task_id,
                    name,
                    description,
                    description_patch,
                    project_id,
                    auto_load,
                    status,
                    metadata,
                )
                .await;
        }
        let m = tasks::WorkItemManager::new(self.store.clone());
        // Plain (non-workflow) task: the document owner is the effective CRUD
        // seat (public tasks are edited as their acting seat).
        let effective_seat = doc.seat_id.clone().unwrap_or_else(|| seat_id.to_string());
        m.update_task(
            &effective_seat,
            task_id,
            name,
            description,
            description_patch,
            project_id,
            auto_load,
            status,
            metadata,
        )
        .await?
        .ok_or_else(|| SlcError::NotFound(format!("task not found: {task_id}")))
    }

    /// Delete a task with full manager semantics: the owner (or public task)
    /// deletes its own plain tasks; a manager deletes plain tasks of every
    /// managed seat. An unknown id is an idempotent `Ok(false)`; an existing
    /// task of a non-managed seat is an explicit
    /// [`SlcError::PermissionDenied`]. Workflow tasks are never deletable —
    /// their event history is preserved ([`SlcError::InvalidInput`]).
    pub async fn task_delete(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let Some(doc) = self.get_document(task_id).await? else {
            return Ok(false);
        };
        if doc.category != DocumentCategory::Task {
            return Ok(false);
        }
        let actor_owns = doc.is_kb_visible(seat_id);
        let actor_manages = doc
            .seat_id
            .as_deref()
            .filter(|owner| *owner != seat_id)
            .is_some_and(|owner| self.can_manage_target(seat_id, owner));
        if !actor_owns && !actor_manages {
            return Err(SlcError::PermissionDenied(format!(
                "task {task_id} belongs to another seat; only its owner or a manager (operator role with an SLC_SEAT_MANAGE_ACL target for that seat) may delete it"
            )));
        }
        if tasks::is_workflow_task(&doc) {
            return Err(SlcError::InvalidInput(
                "workflow tasks cannot be deleted; preserve their event history".into(),
            ));
        }
        self.store().kb_purge(task_id).await
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

    // ── seat roles: managing other seats' context ─────────────────────────

    /// A seat's roles (empty — no special rights).
    pub fn seat_roles(&self, seat_id: &str) -> Vec<roles::SeatRole> {
        self.config
            .seat_roles
            .get(seat_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether an operator has at least one explicitly allowed cross-seat target.
    pub fn can_manage_seats(&self, seat_id: &str) -> bool {
        self.has_operator_role(seat_id)
            && self
                .config
                .seat_manage_acl
                .get(seat_id)
                .is_some_and(|targets| !targets.is_empty())
    }

    fn has_operator_role(&self, seat_id: &str) -> bool {
        self.seat_roles(seat_id)
            .contains(&roles::SeatRole::Operator)
    }

    /// Whether an operator is explicitly allowed to manage this target.
    pub fn can_manage_target(&self, actor: &str, target_seat: &str) -> bool {
        if actor == target_seat {
            return true;
        }
        self.has_operator_role(actor)
            && self
                .config
                .seat_manage_acl
                .get(actor)
                .is_some_and(|targets| targets.contains(target_seat) || targets.contains("*"))
    }

    /// Explicit cross-seat targets exposed to operator clients for discovery.
    pub fn allowed_manage_targets(&self, actor: &str) -> Vec<String> {
        let mut targets = self
            .config
            .seat_manage_acl
            .get(actor)
            .map(|targets| targets.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        targets.sort();
        targets
    }

    /// Whether a seat can read/edit a document: its own/public
    /// (is_kb_visible) or an operator (managing other seats' context).
    pub fn can_read_document(&self, seat_id: &str, doc: &Document) -> bool {
        doc.is_kb_visible(seat_id)
            || doc
                .seat_id
                .as_deref()
                .is_some_and(|owner| self.can_manage_target(seat_id, owner))
    }

    /// Whether a seat may mutate or delete an existing knowledge document.
    /// Public visibility is deliberately not write authority.
    pub fn can_write_document(&self, seat_id: &str, doc: &Document) -> bool {
        match doc.seat_id.as_deref() {
            Some(owner) => self.can_manage_target(seat_id, owner),
            None => self.has_operator_role(seat_id) && self.can_manage_seats(seat_id),
        }
    }

    /// Permission check: actor may manage target_seat (own seat — always).
    pub async fn require_seat_manage(&self, actor: &str, target_seat: &str) -> SlcResult<()> {
        if self.can_manage_target(actor, target_seat) {
            return Ok(());
        }
        Err(SlcError::PermissionDenied(format!(
            "seat {actor} has no right to manage seat {target_seat} (operator role and explicit SLC_SEAT_MANAGE_ACL target required)"
        )))
    }

    /// Activate a document on the target seat (own seat — no role needed).
    pub async fn document_activate_for(
        &self,
        actor: &str,
        target_seat: &str,
        document_id: &str,
    ) -> SlcResult<bool> {
        self.require_seat_manage(actor, target_seat).await?;
        self.document_activate(target_seat, document_id).await
    }

    /// Clear the active document/task on the target seat.
    pub async fn document_deactivate_for(&self, actor: &str, target_seat: &str) -> SlcResult<()> {
        self.require_seat_manage(actor, target_seat).await?;
        self.document_deactivate(target_seat).await
    }

    /// Set the active task for the target seat.
    pub async fn task_activate_for(
        &self,
        actor: &str,
        target_seat: &str,
        task_id: &str,
    ) -> SlcResult<bool> {
        self.require_seat_manage(actor, target_seat).await?;
        self.task_activate(target_seat, task_id).await
    }

    /// Change a project's status (active|archived) on the target seat.
    pub async fn project_set_status_for(
        &self,
        actor: &str,
        target_seat: &str,
        project_id: &str,
        status: &str,
    ) -> SlcResult<Option<tasks::ProjectInfo>> {
        if status != tasks::STATUS_PROJECT_ACTIVE && status != tasks::STATUS_PROJECT_ARCHIVED {
            return Err(SlcError::InvalidInput(format!(
                "status must be {} or {}",
                tasks::STATUS_PROJECT_ACTIVE,
                tasks::STATUS_PROJECT_ARCHIVED
            )));
        }
        self.require_seat_manage(actor, target_seat).await?;
        let m = tasks::WorkItemManager::new(self.store.clone());
        m.update_project(
            target_seat,
            project_id,
            None,
            None,
            None,
            None,
            Some(status),
            None,
        )
        .await
    }

    /// Archive/unarchive a focus on the target seat.
    pub async fn focus_set_archived_for(
        &self,
        actor: &str,
        target_seat: &str,
        focus_id: &str,
        archived: bool,
    ) -> SlcResult<bool> {
        self.require_seat_manage(actor, target_seat).await?;
        let f = focus::FocusManager::new(self.store.clone());
        f.set_archived(focus_id, target_seat, archived).await
    }

    // ── document rename (cascade of links) ────────────────────────────────

    /// Rename a document (any category, including tasks and projects):
    /// changing document_id/file name + cascading link fixes — the
    /// auto_load/references of all documents, task-project links
    /// (metadata.project/project_id), wiki links `[[old]]` in content, and
    /// active seat pointers. `new_name` (for tasks/projects) updates the
    /// human-readable name. Only documents visible to the seat.
    pub async fn rename_document(
        &self,
        seat_id: &str,
        document_id: &str,
        new_id: &str,
        new_name: Option<&str>,
    ) -> SlcResult<RenameReport> {
        if new_id.trim().is_empty() {
            return Err(SlcError::InvalidInput("new_document_id required".into()));
        }
        if new_id == document_id {
            return Err(SlcError::InvalidInput("new id must differ".into()));
        }
        let Some(doc) = self.get_document(document_id).await? else {
            return Err(SlcError::NotFound(format!(
                "document not found: {document_id}"
            )));
        };
        if !doc.is_kb_visible(seat_id) {
            return Err(SlcError::NotFound(format!(
                "document not found: {document_id}"
            )));
        }
        if tasks::is_workflow_task(&doc) {
            return Err(SlcError::InvalidInput(
                "workflow task ids are immutable; preserve event correlation and lineage".into(),
            ));
        }
        if self.store.kb_get(new_id).await?.is_some() {
            return Err(SlcError::InvalidInput(format!(
                "document already exists: {new_id}"
            )));
        }

        let mut links_fixed = 0usize;
        let mut content_links_fixed = 0usize;
        let all = self
            .store
            .kb_find(&DocFilter::default(), &DocSort::default(), 100_000)
            .await?;
        let mut reembed: Vec<String> = Vec::new();
        // Apply changed documents as a batch — one git commit
        // instead of a commit per document (kb_replace_many).
        let mut changed_docs: Vec<Document> = Vec::new();
        for mut d in all {
            let mut changed = false;
            // Reference fields.
            let fix = |ids: &mut Vec<String>| -> bool {
                let mut c = false;
                for id in ids.iter_mut() {
                    if id == document_id {
                        *id = new_id.to_string();
                        c = true;
                    }
                }
                c
            };
            let c1 = fix(&mut d.auto_load);
            let c2 = fix(&mut d.references);
            // Task-project links (metadata.project / project_id).
            let mut c3 = false;
            for key in ["project", "project_id"] {
                if d.metadata.extra.get(key).and_then(|v| v.as_str()) == Some(document_id) {
                    d.metadata.extra.insert(key.into(), json!(new_id));
                    c3 = true;
                }
            }
            // Wiki links [[old]] / [[old|alias]] in content.
            let old_bracket = format!("[[{document_id}]]");
            let old_pipe = format!("[[{document_id}|");
            let mut c4 = false;
            if d.content.contains(&old_bracket) || d.content.contains(&old_pipe) {
                d.content = d
                    .content
                    .replace(&old_bracket, &format!("[[{new_id}]]"))
                    .replace(&old_pipe, &format!("[[{new_id}|"));
                d.content_hash = content_hash(&d.content);
                c4 = true;
            }
            if c1 || c2 || c3 || c4 {
                d.updated_at = chrono::Utc::now();
                d.version += 1;
                changed = true;
                changed_docs.push(d.clone());
            }
            if changed && c4 {
                links_fixed += 1;
                content_links_fixed += 1;
            } else if changed {
                links_fixed += 1;
            }
            if c4 {
                reembed.push(d.document_id);
            }
        }

        // Apply the cascading fixes as one batch (a single git commit).
        if !changed_docs.is_empty() {
            self.store.kb_replace_many(&changed_docs).await?;
        }

        // The rename itself (file/key).
        let ok = self.store.kb_rename(document_id, new_id).await?;
        if !ok {
            return Err(SlcError::NotFound(format!(
                "document not found: {document_id}"
            )));
        }

        // Human-readable name for tasks/projects.
        if let Some(name) = new_name.filter(|n| !n.trim().is_empty()) {
            if matches!(
                doc.category,
                DocumentCategory::Task | DocumentCategory::Project
            ) {
                if let Some(mut d) = self.store.kb_get(new_id).await? {
                    d.metadata.extra.insert("name".into(), json!(name));
                    d.updated_at = chrono::Utc::now();
                    d.version += 1;
                    self.store.kb_replace(&d).await?;
                }
            }
        }

        // Active seat pointers (the unified and the task pointer).
        let mut seats_updated = Vec::new();
        for seat in self.seats.list_active(1000).await? {
            let mut changed = false;
            let mut s = seat;
            if s.active_task_id.as_deref() == Some(document_id) {
                s.active_task_id = Some(new_id.to_string());
                changed = true;
            }
            if s.active_document_id.as_deref() == Some(document_id) {
                s.active_document_id = Some(new_id.to_string());
                changed = true;
            }
            if changed {
                self.store.insert_seat(&s).await?;
                seats_updated.push(s.seat_id);
            }
        }

        // Embeddings: remove the old key (the obsidian sidecar stores them by
        // document_id; the sqlite move is done in kb_rename), then re-embed
        // the new id and the documents with changed content.
        let _ = self.store.delete_embeddings(document_id).await;
        let _ = self.reembed_document(new_id).await;
        for id in reembed {
            let _ = self.reembed_document(&id).await;
        }

        Ok(RenameReport {
            old_id: document_id.to_string(),
            new_id: new_id.to_string(),
            links_fixed,
            content_links_fixed,
            seats_updated,
        })
    }

    /// Rename a task: the new id is a slug from `new_name` (as on
    /// creation), the name is updated, links are fixed cascadingly.
    pub async fn rename_task(
        &self,
        seat_id: &str,
        task_id: &str,
        new_name: &str,
    ) -> SlcResult<RenameReport> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        let new_id = m.speaking_id(new_name).await;
        self.rename_document(seat_id, task_id, &new_id, Some(new_name))
            .await
    }

    /// Rename a project: the new id is a slug from `new_name`, the name is updated.
    pub async fn rename_project(
        &self,
        seat_id: &str,
        project_id: &str,
        new_name: &str,
    ) -> SlcResult<RenameReport> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        let new_id = m.speaking_id(new_name).await;
        self.rename_document(seat_id, project_id, &new_id, Some(new_name))
            .await
    }

    /// The seat's active document, if any (task fallback included).
    pub async fn document_get_active(&self, seat_id: &str) -> SlcResult<Option<Document>> {
        let Some(id) = self.store.get_seat_active_document(seat_id).await? else {
            return Ok(None);
        };
        Ok(self
            .get_document(&id)
            .await?
            .filter(|doc| self.can_read_document(seat_id, doc)))
    }

    pub async fn task_get_active(&self, seat_id: &str) -> SlcResult<Option<tasks::TaskInfo>> {
        let m = tasks::WorkItemManager::new(self.store.clone());
        let Some(task) = m.get_active_task(seat_id).await? else {
            return Ok(None);
        };
        match self.get_document(&task.task_id).await? {
            Some(doc) if self.can_read_document(seat_id, &doc) => Ok(Some(task)),
            _ => Ok(None),
        }
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
        description_patch: Option<&serde_json::Value>,
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
            description_patch,
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

    /// Re-read the backing store from disk (multi-process sync).
    pub async fn refresh(&self) -> SlcResult<()> {
        self.store.refresh().await
    }

    /// Start the background timer scheduler: default per-seat timers +
    /// compression/consolidation handlers. Idempotent. The MCP server calls
    /// this on startup.
    pub async fn start_background(&self) -> SlcResult<()> {
        // Initialization awaits storage and timer operations, so this must be
        // an async-aware mutex. A std::sync::MutexGuard here used to be held
        // across every await and could stall the whole executor.
        let mut guard = self.scheduler.lock().await;
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

        // Multi-process sync: periodically re-read the store from disk so
        // changes made by another service sharing the same vault (e.g. the
        // web UI) become visible. SLC_VAULT_REFRESH_SECS: interval (default
        // 30s, 0 = off).
        let refresh_secs = std::env::var("SLC_VAULT_REFRESH_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        if refresh_secs > 0 {
            let store = self.store.clone();
            tokio::spawn(async move {
                let mut ticker =
                    tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
                loop {
                    ticker.tick().await;
                    if let Err(e) = store.refresh().await {
                        tracing::debug!("store refresh failed: {e}");
                    }
                }
            });
        }
        Ok(())
    }

    pub async fn scheduler(&self) -> Option<timer::TimerRegistry> {
        self.scheduler.lock().await.clone()
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
    use serde_json::{Value, json};

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

    #[tokio::test]
    async fn active_anchors_do_not_disclose_foreign_private_documents() {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(MockLlm::new(vec![]));
        let engine = SlcEngine::with(store, llm, SlcConfig::default());
        engine.ensure_seat("reader").await.unwrap();
        engine.ensure_seat("owner").await.unwrap();
        for (id, owner, visible) in [
            ("private", Some("owner"), false),
            ("own", Some("reader"), true),
            ("public", None, true),
        ] {
            let mut doc = Document::new(
                id,
                DocumentCategory::Custom,
                "content",
                DocMeta::default(),
                vec![],
                owner.map(str::to_string),
            );
            engine.add_document(&mut doc).await.unwrap();
            // Simulate an old pointer written before activation ACL enforcement.
            engine
                .seats
                .set_active_document("reader", Some(id))
                .await
                .unwrap();
            assert_eq!(
                engine
                    .document_get_active("reader")
                    .await
                    .unwrap()
                    .is_some(),
                visible,
                "{id}"
            );
        }
        let task = engine
            .task_create(
                "owner",
                "private task",
                "private content",
                None,
                &[],
                &json!({}),
            )
            .await
            .unwrap();
        engine
            .seats
            .set_active_document("reader", None)
            .await
            .unwrap();
        engine
            .seats
            .set_active_task("reader", Some(&task.task_id), None)
            .await
            .unwrap();
        assert!(engine.task_get_active("reader").await.unwrap().is_none());
        assert!(
            engine
                .document_get_active("reader")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn operator_and_explicit_target_are_both_required() {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(MockLlm::new(vec![]));
        let config = SlcConfig::default()
            .with_seat_role("role-only", roles::SeatRole::Operator)
            .with_seat_manage_target("acl-only", "worker")
            .with_seat_role("boss", roles::SeatRole::Operator)
            .with_seat_manage_target("boss", "worker");
        let engine = SlcEngine::with(store, llm, config);
        assert!(!engine.can_manage_target("role-only", "worker"));
        assert!(!engine.can_manage_target("acl-only", "worker"));
        assert!(engine.can_manage_target("boss", "worker"));
        assert!(!engine.can_manage_target("boss", "unlisted"));
        assert!(engine.can_manage_target("worker", "worker"));
        assert!(
            engine
                .require_seat_manage("role-only", "worker")
                .await
                .is_err()
        );
        assert!(
            engine
                .require_seat_manage("acl-only", "worker")
                .await
                .is_err()
        );
        assert!(engine.require_seat_manage("boss", "worker").await.is_ok());
    }

    /// Seat roles: an operator manages another seat's context, a regular
    /// seat — not (SlcError::PermissionDenied).
    #[tokio::test]
    async fn seat_roles_manage_other_seats() {
        use crate::error::SlcError;
        use crate::roles::SeatRole;

        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(MockLlm::new(vec![]));
        // The operator — seat "boss"; a regular seat — "worker".
        let config = SlcConfig::default()
            .with_seat_role("boss", SeatRole::Operator)
            .with_seat_manage_target("boss", "worker");
        let engine = SlcEngine::with(store.clone(), llm, config);

        engine.ensure_seat("boss").await.unwrap();
        engine.ensure_seat("worker").await.unwrap();

        // The document and task of seat worker.
        let mut doc = Document::new(
            "doc_worker",
            DocumentCategory::Custom,
            "worker doc",
            DocMeta::default(),
            vec![],
            Some("worker".to_string()),
        );
        engine.add_document(&mut doc).await.unwrap();
        let task = engine
            .task_create("worker", "worker task", "", None, &[], &json!({}))
            .await
            .unwrap();

        // A regular seat cannot manage another seat.
        let err = engine
            .document_activate_for("worker", "boss", "doc_worker")
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::PermissionDenied(_)), "{err:?}");

        // Own seat — always allowed (target = actor).
        assert!(
            engine
                .document_activate_for("worker", "worker", "doc_worker")
                .await
                .unwrap()
        );

        // The operator activates/deactivates another seat.
        assert!(
            engine
                .document_activate_for("boss", "worker", "doc_worker")
                .await
                .unwrap()
        );
        assert_eq!(
            engine
                .document_get_active("worker")
                .await
                .unwrap()
                .unwrap()
                .document_id,
            "doc_worker"
        );
        engine
            .document_deactivate_for("boss", "worker")
            .await
            .unwrap();
        assert!(
            engine
                .document_get_active("worker")
                .await
                .unwrap()
                .is_none()
        );

        // Tasks: the operator sets an active task on another seat.
        assert!(
            engine
                .task_activate_for("boss", "worker", &task.task_id)
                .await
                .unwrap()
        );
        assert_eq!(
            engine
                .task_get_active("worker")
                .await
                .unwrap()
                .unwrap()
                .task_id,
            task.task_id
        );

        // Focuses: the operator archives a focus of another seat.
        let f = engine
            .focus_add("worker", "focus task", "", 5, &[], None)
            .await
            .unwrap();
        assert!(
            engine
                .focus_set_archived_for("boss", "worker", &f.focus_id, true)
                .await
                .unwrap()
        );
        assert!(
            engine
                .focus_list("worker", None)
                .await
                .unwrap()
                .iter()
                .all(|x| x.focus_id != f.focus_id || x.archived)
        );
        // Without a role — denied.
        let err = engine
            .focus_set_archived_for("worker", "boss", &f.focus_id, true)
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::PermissionDenied(_)));

        // Projects: the operator archives another seat's project.
        let proj = engine
            .project_create("worker", "worker proj", "", &[], &json!({}))
            .await
            .unwrap();
        let updated = engine
            .project_set_status_for("boss", "worker", &proj.project_id, "archived")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, "archived");
        // Invalid status — InvalidInput.
        let err = engine
            .project_set_status_for("boss", "worker", &proj.project_id, "bogus")
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::InvalidInput(_)));

        // seat_roles returns the configuration.
        assert_eq!(engine.seat_roles("boss"), vec![SeatRole::Operator]);
        assert!(engine.can_manage_seats("boss"));
        assert!(engine.can_manage_target("boss", "worker"));
        assert!(!engine.can_manage_target("boss", "unrelated"));
        assert!(!engine.can_manage_seats("worker"));
    }

    #[tokio::test]
    async fn document_write_authority_is_not_inferred_from_visibility() {
        use crate::roles::SeatRole;

        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(MockLlm::new(vec![]));
        let config = SlcConfig::default()
            .with_seat_role("boss", SeatRole::Operator)
            .with_seat_manage_target("boss", "worker")
            .with_seat_role("unscoped-operator", SeatRole::Operator);
        let engine = SlcEngine::with(store, llm, config);
        let owned = Document::new(
            "owned",
            DocumentCategory::Custom,
            "content",
            DocMeta::default(),
            vec![],
            Some("worker".into()),
        );
        let public = Document::new(
            "public",
            DocumentCategory::System,
            "content",
            DocMeta::default(),
            vec![],
            None,
        );

        assert!(engine.can_write_document("worker", &owned));
        assert!(engine.can_write_document("boss", &owned));
        assert!(!engine.can_write_document("other", &owned));
        assert!(engine.can_write_document("boss", &public));
        assert!(!engine.can_write_document("worker", &public));
        assert!(!engine.can_write_document("unscoped-operator", &public));
    }

    /// A manager (operator + SLC_SEAT_MANAGE_ACL) updates the contactor's
    /// workflow task in IN_WORK: the edit is persisted through the canonical
    /// workflow-store route (a durable update event) and survives a reload.
    #[tokio::test]
    async fn manager_updates_workflow_task_of_managed_seat_with_durable_event() {
        use crate::roles::SeatRole;
        use crate::tasks::STATUS_ACTIVE;
        use crate::workflow::{QUEUE_STATE_RUNNING, TaskEventKind};
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;

        let store: Arc<dyn StorageBackend> =
            Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let config = SlcConfig::default()
            .with_seat_role("seat-manager", SeatRole::Operator)
            .with_seat_manage_target("seat-manager", "seat-contactor")
            .with_seat_manage_target("seat-manager", "seat-analyst");
        let mut config = config;
        config.principal_seats = HashMap::from([
            ("manager".into(), "seat-manager".into()),
            ("contactor".into(), "seat-contactor".into()),
            ("analyst".into(), "seat-analyst".into()),
        ]);
        config.task_assign_acl = HashMap::from([("manager".into(), HashSet::from(["*".into()]))]);
        let engine = SlcEngine::with(store.clone(), Arc::new(MockLlm::new(vec![])), config);

        let assigned = engine
            .workflow_assign_task(
                "seat-manager",
                "contactor",
                "Prepare pitch",
                "initial body",
                None,
                None,
                &[],
                &json!({"customer": "acme"}),
                Some("assign-1"),
            )
            .await
            .unwrap();
        assert_eq!(assigned.issuer.as_deref(), Some("manager"));
        assert_eq!(assigned.assignee.as_deref(), Some("contactor"));
        engine
            .workflow_start_task(
                "seat-contactor",
                &assigned.task_id,
                "Starting",
                Some("start-1"),
            )
            .await
            .unwrap();

        // The manager edits the contactor's workflow task while it is
        // IN_WORK: name + append-diff to the body + auto_load + project + metadata.
        let updated = engine
            .task_update(
                "seat-manager",
                &assigned.task_id,
                Some("Prepare pitch v2"),
                None,
                Some(&json!([{"op":"append","content":"## Правки менеджера\nуточнено"}])),
                Some(Some("lead_gen")),
                Some(&["swarm_policy_contactor".to_string()]),
                Some("IN_WORK"),
                Some(&json!({"priority": "high"})),
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "Prepare pitch v2");
        assert!(updated.description.contains("уточнено"));
        assert_eq!(updated.status, STATUS_ACTIVE);
        assert_eq!(updated.queue_state.as_deref(), Some(QUEUE_STATE_RUNNING));
        assert_eq!(updated.project_id.as_deref(), Some("lead_gen"));
        assert!(
            updated
                .auto_load
                .contains(&"swarm_policy_contactor".to_string())
        );
        assert_eq!(updated.metadata["priority"], json!("high"));

        // Reload: a new engine instance on the same store sees the edit
        // both from the contactor seat and from the manager seat.
        let engine2 = SlcEngine::with(store.clone(), Arc::new(MockLlm::new(vec![])), {
            let mut c = SlcConfig::default()
                .with_seat_role("seat-manager", SeatRole::Operator)
                .with_seat_manage_target("seat-manager", "seat-contactor");
            c.principal_seats = HashMap::from([
                ("manager".into(), "seat-manager".into()),
                ("contactor".into(), "seat-contactor".into()),
            ]);
            c.task_assign_acl = HashMap::from([("manager".into(), HashSet::from(["*".into()]))]);
            c
        });
        let from_owner = engine2
            .workflow_get_task("seat-contactor", &assigned.task_id)
            .await
            .unwrap();
        assert_eq!(from_owner.name, "Prepare pitch v2");
        assert!(from_owner.description.contains("уточнено"));
        let from_manager = engine2
            .workflow_get_task("seat-manager", &assigned.task_id)
            .await
            .unwrap();
        assert_eq!(from_manager.name, "Prepare pitch v2");
        // Identity and queue are untouched.
        let raw = store.kb_get(&assigned.task_id).await.unwrap().unwrap();
        assert_eq!(
            raw.metadata
                .extra
                .get("workflow_version")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            raw.metadata
                .extra
                .get("queue_state")
                .and_then(Value::as_str),
            Some(QUEUE_STATE_RUNNING)
        );
        assert!(raw.content.contains("уточнено"));

        // Audit: a durable update event carrying the changed fields.
        let events = engine2
            .workflow_task_events("seat-manager", &assigned.task_id, 50)
            .await
            .unwrap();
        let update_event = events
            .iter()
            .find(|event| event.kind == TaskEventKind::Update)
            .expect("manager update must append a durable update event");
        assert_eq!(update_event.actor, "manager");
        assert!(update_event.message.contains("updated by manager"));
        let fields = update_event.metadata["fields_changed"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        for field in ["name", "description", "auto_load", "project_id", "metadata"] {
            assert!(
                fields.contains(&field),
                "fields_changed must include {field}: {fields:?}"
            );
        }

        // A workflow task's status is not rewritten through events — an explicit error.
        let status_err = engine
            .task_update(
                "seat-manager",
                &assigned.task_id,
                None,
                None,
                None,
                None,
                None,
                Some("COMPLETED"),
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(status_err, SlcError::InvalidInput(_)),
            "{status_err}"
        );
        assert!(status_err.to_string().contains("event-governed"));

        // The owner (contactor) stays on the append-only contract.
        let owner_err = engine
            .task_update(
                "seat-contactor",
                &assigned.task_id,
                Some("hijacked"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(owner_err.to_string().contains("append-only"), "{owner_err}");
    }

    /// Tasks of other seats (plain and workflow) cannot be updated or
    /// deleted by a non-manager: an explicit PermissionDenied instead of a fake "not found".
    #[tokio::test]
    async fn non_manager_cannot_update_or_delete_tasks_of_other_seats() {
        use crate::roles::SeatRole;
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;

        let store: Arc<dyn StorageBackend> =
            Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let config = SlcConfig::default()
            .with_seat_role("seat-manager", SeatRole::Operator)
            .with_seat_manage_target("seat-manager", "seat-contactor")
            .with_seat_manage_target("seat-manager", "seat-analyst");
        let mut config = config;
        config.principal_seats = HashMap::from([
            ("manager".into(), "seat-manager".into()),
            ("contactor".into(), "seat-contactor".into()),
            ("analyst".into(), "seat-analyst".into()),
        ]);
        config.task_assign_acl = HashMap::from([("manager".into(), HashSet::from(["*".into()]))]);
        let engine = SlcEngine::with(store, Arc::new(MockLlm::new(vec![])), config);
        engine.ensure_seat("seat-outsider").await.unwrap();

        let plain = engine
            .task_create("seat-contactor", "private plan", "", None, &[], &json!({}))
            .await
            .unwrap();
        let workflow = engine
            .workflow_assign_task(
                "seat-manager",
                "contactor",
                "Call campaign",
                "call list",
                None,
                None,
                &[],
                &json!({}),
                Some("deny-assign"),
            )
            .await
            .unwrap();

        // A non-manager cannot update another seat's plain task.
        let err = engine
            .task_update(
                "seat-outsider",
                &plain.task_id,
                Some("rewritten"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::PermissionDenied(_)), "{err}");
        assert!(err.to_string().contains("SLC_SEAT_MANAGE_ACL"));
        // The analyst does not own the contactor's task either.
        assert!(matches!(
            engine
                .task_update(
                    "seat-analyst",
                    &plain.task_id,
                    Some("rewritten"),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await,
            Err(SlcError::PermissionDenied(_))
        ));
        // Another seat's workflow task cannot be deleted…
        let err = engine
            .task_delete("seat-outsider", &workflow.task_id)
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::PermissionDenied(_)), "{err}");
        // …nor updated.
        let err = engine
            .task_update(
                "seat-outsider",
                &workflow.task_id,
                Some("x"),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::PermissionDenied(_)), "{err}");
        // Another seat's plain task cannot be deleted.
        let err = engine
            .task_delete("seat-analyst", &plain.task_id)
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::PermissionDenied(_)), "{err}");
        // The owner deletes its own plain task.
        assert!(
            engine
                .task_delete("seat-contactor", &plain.task_id)
                .await
                .unwrap()
        );
        assert!(engine.get_document(&plain.task_id).await.unwrap().is_none());
        // An existing workflow task cannot be deleted by its owner either (event history).
        let err = engine
            .task_delete("seat-contactor", &workflow.task_id)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be deleted"), "{err}");
    }

    /// A manager sees/reads/deletes tasks of all managed seats
    /// (plain and workflow, including IN_WORK); an unknown id — NotFound,
    /// workflow tasks — cannot be deleted.
    #[tokio::test]
    async fn manager_get_list_delete_semantics_cover_all_managed_seats() {
        use crate::roles::SeatRole;
        use crate::workflow::TaskListScope;
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;

        let store: Arc<dyn StorageBackend> =
            Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let config = SlcConfig::default()
            .with_seat_role("seat-manager", SeatRole::Operator)
            .with_seat_manage_target("seat-manager", "seat-contactor")
            .with_seat_manage_target("seat-manager", "seat-analyst");
        let mut config = config;
        config.principal_seats = HashMap::from([
            ("manager".into(), "seat-manager".into()),
            ("contactor".into(), "seat-contactor".into()),
            ("analyst".into(), "seat-analyst".into()),
        ]);
        config.task_assign_acl = HashMap::from([("manager".into(), HashSet::from(["*".into()]))]);
        let engine = SlcEngine::with(store.clone(), Arc::new(MockLlm::new(vec![])), config);

        let plain_contactor = engine
            .task_create(
                "seat-contactor",
                "contactor todo",
                "",
                None,
                &[],
                &json!({}),
            )
            .await
            .unwrap();
        let plain_analyst = engine
            .task_create("seat-analyst", "analyst todo", "", None, &[], &json!({}))
            .await
            .unwrap();
        let workflow = engine
            .workflow_assign_task(
                "seat-manager",
                "contactor",
                "Cold list",
                "call them",
                None,
                None,
                &[],
                &json!({}),
                Some("list-assign"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-contactor",
                &workflow.task_id,
                "go",
                Some("list-start"),
            )
            .await
            .unwrap();

        // get: the manager reads tasks of every managed seat (including the
        // IN_WORK workflow task), the owner — its own.
        let via_manager = engine
            .workflow_get_task("seat-manager", &plain_analyst.task_id)
            .await
            .unwrap();
        assert_eq!(via_manager.task_id, plain_analyst.task_id);
        let in_work = engine
            .workflow_get_task("seat-manager", &workflow.task_id)
            .await
            .unwrap();
        assert_eq!(in_work.status, crate::tasks::STATUS_ACTIVE);
        assert_eq!(
            in_work.queue_state.as_deref(),
            Some(crate::workflow::QUEUE_STATE_RUNNING)
        );

        // list (Visible scope): workflow tasks of all managed seats
        // (including IN_WORK) are visible to the manager in one call.
        let visible = engine
            .workflow_list_tasks(
                "seat-manager",
                TaskListScope::Visible,
                None,
                None,
                None,
                None,
                50,
            )
            .await
            .unwrap();
        assert!(
            visible.iter().any(|t| t.task_id == workflow.task_id),
            "manager Visible list must include the IN_WORK workflow task: {:?}",
            visible.iter().map(|t| &t.task_id).collect::<Vec<_>>()
        );
        // The per-seat list also covers the plain tasks of every managed
        // seat (legacy target_seat manager semantics).
        let per_seat_contactor = engine
            .task_list("seat-contactor", None, None, 50)
            .await
            .unwrap();
        let contactor_ids: HashSet<String> = per_seat_contactor
            .iter()
            .map(|t| t.task_id.clone())
            .collect();
        assert!(contactor_ids.contains(&plain_contactor.task_id));
        assert!(contactor_ids.contains(&workflow.task_id));
        let per_seat_analyst = engine
            .task_list("seat-analyst", None, None, 50)
            .await
            .unwrap();
        assert!(
            per_seat_analyst
                .iter()
                .any(|t| t.task_id == plain_analyst.task_id)
        );
        // An outsider sees nothing foreign in the Visible scope.
        let outsider_visible = engine
            .workflow_list_tasks(
                "seat-outsider",
                TaskListScope::Visible,
                None,
                None,
                None,
                None,
                50,
            )
            .await
            .unwrap();
        assert!(outsider_visible.is_empty());

        // delete: the manager deletes a plain task of a managed seat…
        assert!(
            engine
                .task_delete("seat-manager", &plain_analyst.task_id)
                .await
                .unwrap()
        );
        assert!(
            engine
                .get_document(&plain_analyst.task_id)
                .await
                .unwrap()
                .is_none()
        );
        // …but not a workflow task (event history is preserved).
        let err = engine
            .task_delete("seat-manager", &workflow.task_id)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be deleted"), "{err}");
        assert!(
            engine
                .get_document(&workflow.task_id)
                .await
                .unwrap()
                .is_some()
        );

        // An unknown id — an honest NotFound, not a "hidden" answer.
        let err = engine
            .task_update(
                "seat-manager",
                "task_no_such_id",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::NotFound(_)), "{err}");
    }

    /// Renaming a document: a cascade over auto_load/references/wiki links/
    /// active seat pointers; rename_task builds the slug from the new name.
    #[tokio::test]
    async fn rename_document_cascades_links() {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(MockLlm::new(vec![]));
        let engine = SlcEngine::with(store.clone(), llm, SlcConfig::default());
        engine.ensure_seat("seat_a").await.unwrap();

        let mut doc = Document::new(
            "old_name",
            DocumentCategory::Custom,
            "текст со ссылкой [[old_name]] и [[old_name|alias]]",
            DocMeta::default(),
            vec![],
            Some("seat_a".to_string()),
        );
        engine.add_document(&mut doc).await.unwrap();
        // The referencing document.
        let mut ref_doc = Document::new(
            "ref_doc",
            DocumentCategory::Custom,
            "ссылки",
            DocMeta::default(),
            vec![],
            Some("seat_a".to_string()),
        );
        ref_doc.auto_load = vec!["old_name".into()];
        ref_doc.references = vec!["old_name".into()];
        engine.add_document(&mut ref_doc).await.unwrap();
        // The seat's active pointer on the old id.
        engine
            .document_activate("seat_a", "old_name")
            .await
            .unwrap();

        let report = engine
            .rename_document("seat_a", "old_name", "new_name", None)
            .await
            .unwrap();
        assert_eq!(report.new_id, "new_name");
        assert_eq!(report.links_fixed, 2); // the doc itself (content) + ref_doc
        assert_eq!(report.content_links_fixed, 1); // only the doc itself
        assert!(report.seats_updated.contains(&"seat_a".to_string()));

        // The old id is gone, the new one is in place, content has replaced links.
        assert!(engine.get_document("old_name").await.unwrap().is_none());
        let renamed = engine.get_document("new_name").await.unwrap().unwrap();
        assert_eq!(
            renamed.content,
            "текст со ссылкой [[new_name]] и [[new_name|alias]]"
        );
        // The seat's pointer was updated.
        assert_eq!(
            engine
                .document_get_active("seat_a")
                .await
                .unwrap()
                .unwrap()
                .document_id,
            "new_name"
        );
        // References updated.
        let refd = engine.get_document("ref_doc").await.unwrap().unwrap();
        assert_eq!(refd.auto_load, vec!["new_name".to_string()]);
        assert_eq!(refd.references, vec!["new_name".to_string()]);
        // A link to a nonexistent id — an error.
        let err = engine
            .rename_document("seat_a", "new_name", "new_name", None)
            .await
            .unwrap_err();
        assert!(matches!(err, SlcError::InvalidInput(_)));
        // Embeddings: the old key was removed, the new one is re-embedded.
        assert!(
            engine
                .store()
                .get_embedding("new_name")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            engine
                .store()
                .get_embedding("old_name")
                .await
                .unwrap()
                .is_none()
        );
    }

    /// rename_task: the new id = slug from the name, the name is updated.
    #[tokio::test]
    async fn rename_task_builds_slug() {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(storage::sqlite::SqliteStore::in_memory().unwrap());
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(MockLlm::new(vec![]));
        let engine = SlcEngine::with(store.clone(), llm, SlcConfig::default());
        engine.ensure_seat("seat_a").await.unwrap();
        let task = engine
            .task_create("seat_a", "Старая задача", "", None, &[], &json!({}))
            .await
            .unwrap();

        let report = engine
            .rename_task("seat_a", &task.task_id, "Новая задача")
            .await
            .unwrap();
        assert_eq!(report.new_id, "novaya_zadacha");
        assert!(engine.get_document(&task.task_id).await.unwrap().is_none());
        let t = engine.task_list("seat_a", None, None, 10).await.unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].task_id, "novaya_zadacha");
        assert_eq!(t[0].name, "Новая задача");
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
                Ok(n) if n > 0 => {
                    tracing::info!(removed = n, "expired paginated responses cleaned")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("pagination cleanup: {e}"),
            }
        }
    });
}
