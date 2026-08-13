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

pub mod error;
pub mod llm;
pub mod memory;
pub mod model;
pub mod search;
pub mod seat;
pub mod storage;

pub use error::{SlcError, SlcResult};
pub use llm::{LlmClient, MockLlm, OllamaClient};
pub use memory::{ConsolidationReport, CompressionReport, HistoryCompressor, MemoryConsolidator};
pub use model::*;
pub use search::{RankWeights, SearchHit, SearchService};
pub use seat::SeatManager;
pub use storage::{DocFilter, DocSort, MetaPatch, SortDir, SortField, StorageBackend};

use std::path::Path;

/// Which storage backend to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageKind {
    /// Obsidian vault — DEFAULT (markdown + frontmatter, folder-organized).
    ObsidianVault,
    /// Embedded SQLite file.
    Sqlite,
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
    pub semantic_weight: f32,
    pub text_weight: f32,
    pub seat_ttl_seconds: i64,
}

impl Default for SlcConfig {
    fn default() -> Self {
        SlcConfig {
            storage: StorageKind::ObsidianVault, // DEFAULT = Obsidian vault
            path: std::env::var("SLC_VAULT_PATH").unwrap_or_else(|_| "~/.slc/vault".into()),
            auto_git_commit: std::env::var("OBSIDIAN_AUTO_GIT_COMMIT").is_ok_and(|v| v == "true"),
            ollama_endpoint: std::env::var("OLLAMA_ENDPOINT").unwrap_or_else(|_| "http://localhost:11434".into()),
            ollama_reasoning_model: std::env::var("OLLAMA_REASONING_MODEL").unwrap_or_else(|_| "gemma3:latest".into()),
            ollama_embedding_model: std::env::var("OLLAMA_EMBEDDING_MODEL").unwrap_or_else(|_| "bge-m3".into()),
            semantic_weight: 0.7,
            text_weight: 0.3,
            seat_ttl_seconds: 86400,
        }
    }
}

/// High-level engine facade — what the app (vs-memory) and the MCP binary use.
pub struct SlcEngine {
    store: std::sync::Arc<dyn StorageBackend>,
    llm: std::sync::Arc<dyn LlmClient>,
    pub seats: SeatManager<std::sync::Arc<dyn StorageBackend>>,
    search: SearchService,
    compressor: HistoryCompressor<std::sync::Arc<dyn StorageBackend>, std::sync::Arc<dyn LlmClient>>,
    consolidator: MemoryConsolidator<std::sync::Arc<dyn StorageBackend>, std::sync::Arc<dyn LlmClient>>,
    pub config: SlcConfig,
}

impl SlcEngine {
    pub fn open(config: SlcConfig) -> SlcResult<Self> {
        let store: std::sync::Arc<dyn StorageBackend> = match config.storage {
            StorageKind::ObsidianVault => {
                let path = expand_tilde(&config.path);
                std::sync::Arc::new(storage::obsidian::ObsidianVaultStore::open(path, config.auto_git_commit)?)
            }
            StorageKind::Sqlite => {
                let path = expand_tilde(&config.path);
                std::sync::Arc::new(storage::sqlite::SqliteStore::open(path)?)
            }
        };
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(OllamaClient::new(
            &config.ollama_endpoint,
            &config.ollama_reasoning_model,
            &config.ollama_embedding_model,
        ));
        Ok(Self::with(store, llm, config))
    }

    /// Build from already-constructed parts (tests, embedding into vs-memory
    /// with a custom LlmClient).
    pub fn with(
        store: std::sync::Arc<dyn StorageBackend>,
        llm: std::sync::Arc<dyn LlmClient>,
        config: SlcConfig,
    ) -> Self {
        let seats = SeatManager::new(store.clone(), config.seat_ttl_seconds);
        let search = SearchService::new(store.clone(), llm.clone(), config.semantic_weight, config.text_weight);
        let compressor = HistoryCompressor::new(store.clone(), llm.clone());
        let consolidator = MemoryConsolidator::new(store.clone(), llm.clone());
        SlcEngine { store, llm, seats, search, compressor, consolidator, config }
    }

    pub fn store(&self) -> &dyn StorageBackend {
        self.store.as_ref()
    }

    // ── knowledge base (RAG-eligible) ────────────────────────────

    pub async fn add_document(&self, doc: &Document) -> SlcResult<()> {
        self.store.kb_insert(doc).await
    }

    pub async fn get_document(&self, document_id: &str) -> SlcResult<Option<Document>> {
        self.store.kb_get(document_id).await
    }

    pub async fn search(&self, query: &str, seat_id: Option<&str>, limit: usize) -> SlcResult<Vec<SearchHit>> {
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
        let f = DocFilter { seat_id: Some(seat_id.into()), ..Default::default() };
        self.store.episodic_find(&f, &DocSort::by_created(SortDir::Desc), limit).await
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
pub unsafe extern "C" fn slc_remember(engine: *const SlcEngine, seat: *const std::os::raw::c_char, event_id: *const std::os::raw::c_char, content: *const std::os::raw::c_char) -> i32 {
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
