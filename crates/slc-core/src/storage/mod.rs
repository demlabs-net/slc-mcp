//! Storage abstraction — typed backend trait for the memory engine.
//!
//! Two SEPARATE stores by design (user-approved):
//!
//! - **KB** (`kb_*`): knowledge documents (core/module/task/project/…).
//!   These are RAG-eligible: embedded, searched (hybrid), linked.
//! - **Episodic** (`episodic_*`): HISTORY docs only. They are NEVER embedded
//!   and NEVER appear in knowledge search; they are processed by the memory
//!   pipeline (progressive summarization L1→L4, consolidation) and queried
//!   by their own retrieval (episodic recall).
//!
//! The legacy Python code kept everything in one Mongo collection; the
//! separation here makes "history is not RAG'd" an invariant of the type
//! system, not a filter that can be forgotten.

pub mod mongodb;
pub mod obsidian;
pub mod sqlite;

use crate::error::{SlcError, SlcResult};
use crate::model::{
    DocLevel, Document, DocumentCategory, EmbeddingRecord, EmbeddingScope, PersistedTimer, Seat,
    SeatStatus,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use std::{collections::BTreeMap, sync::Arc};

/// Filter for document queries — typed subset the engine actually uses.
#[derive(Debug, Clone, Default)]
pub struct DocFilter {
    /// Explicit document-id allowlist (compression marks exactly the docs it
    /// consumed — filtering by predicates alone used to over-mark).
    pub document_ids: Option<Vec<String>>,
    pub category: Option<DocumentCategory>,
    /// Exact owner match (`None` = unfiltered).
    pub seat_id: Option<String>,
    /// Visibility predicate: public (no owner) OR owned by this seat.
    pub visible_to: Option<String>,
    pub doc_level: Option<DocLevel>,
    pub doc_type: Option<String>,
    /// Exact string matches inside `metadata.extra`. This keeps workflow
    /// queue queries selective before the backend applies their result limit.
    pub extra_strings: BTreeMap<String, String>,
    /// Exclude documents whose string-valued `metadata.extra` field is in the
    /// supplied set. Missing/non-string values are retained.
    pub extra_strings_not_in: BTreeMap<String, Vec<String>>,
    /// `Some(true)` = archived; `Some(false)` = explicitly not archived.
    pub archived: Option<bool>,
    /// `Some(true)` = `consolidated != true` (not yet consolidated).
    pub not_consolidated: bool,
    /// `Some(true)` = `compression_batch_id` present; `Some(false)` = absent.
    pub has_compression_batch: Option<bool>,
    pub tags_any: Vec<String>,
    pub tags_all: Vec<String>,
    pub deleted: bool,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

impl DocFilter {
    /// Visibility: public docs (no owner) or owned by `seat_id`.
    pub fn visible_to(seat_id: &str) -> Self {
        let mut f = DocFilter::default();
        f.visible_to = Some(seat_id.to_string());
        f
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortField {
    #[default]
    CreatedAt,
    UpdatedAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DocSort {
    pub field: SortField,
    pub dir: SortDir,
}

impl DocSort {
    pub fn by_created(dir: SortDir) -> Self {
        Self {
            field: SortField::CreatedAt,
            dir,
        }
    }
    pub fn by_updated(dir: SortDir) -> Self {
        Self {
            field: SortField::UpdatedAt,
            dir,
        }
    }
}

/// Metadata flag updates applied together (compression marks sources).
#[derive(Debug, Clone, Default)]
pub struct MetaPatch {
    pub set_archived: Option<bool>,
    pub set_consolidated: Option<bool>,
    pub set_compression_batch_id: Option<String>,
}

/// Compact KB projection for scans that only need identity and metadata.
/// Avoids cloning or transferring document bodies, tags, and dependency lists.
#[derive(Debug, Clone)]
pub struct KbMetadataProjection {
    pub document_id: String,
    pub category: DocumentCategory,
    pub seat_id: Option<String>,
    pub metadata_extra: Map<String, Value>,
}

/// The engine's storage contract. [`sqlite::SqliteStore`] is the embedded
/// default (used by both the standalone binary and the app-embedded static
/// lib).
#[async_trait]
pub trait StorageBackend: Send + Sync {
    // ── KB documents (RAG-eligible) ──────────────────────────────
    async fn kb_insert(&self, doc: &Document) -> SlcResult<()>;
    async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>>;
    /// Update content (bumps version/updated_at, recomputes hash).
    async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool>;
    /// Insert if absent, else update content (upsert semantics).
    async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool>;
    /// Full replace: insert or overwrite the whole document (metadata, tags,
    /// auto_load, content — everything). Returns true if it replaced existing.
    async fn kb_replace(&self, doc: &Document) -> SlcResult<bool>;
    /// Batch document replacement: one git commit per whole batch
    /// (obsidian); default — a loop over [`StorageBackend::kb_replace`].
    async fn kb_replace_many(&self, docs: &[Document]) -> SlcResult<()> {
        for d in docs {
            self.kb_replace(d).await?;
        }
        Ok(())
    }

    /// Rename a document (change document_id/file name). Cascading
    /// references (auto_load/references/seat pointers) are fixed by the engine.
    /// Default: insert under the new id + purge the old one; obsidian/sqlite
    /// override this atomically.
    async fn kb_rename(&self, old_id: &str, new_id: &str) -> SlcResult<bool> {
        let Some(mut doc) = self.kb_get(old_id).await? else {
            return Ok(false);
        };
        if self.kb_get(new_id).await?.is_some() {
            return Err(SlcError::Storage(format!(
                "document already exists: {new_id}"
            )));
        }
        doc.document_id = new_id.to_string();
        doc.updated_at = chrono::Utc::now();
        doc.version += 1;
        self.kb_insert(&doc).await?;
        self.kb_purge(old_id).await?;
        Ok(true)
    }
    async fn kb_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>>;
    /// Return every matching KB document. The compatibility default preserves
    /// the pre-existing `kb_find(..., usize::MAX)` convention for downstream
    /// backends; built-in backends override it with native unbounded queries.
    async fn kb_find_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<Document>> {
        self.kb_find(filter, sort, usize::MAX).await
    }
    /// Return every matching KB identity/metadata projection. The compatibility
    /// default derives it from [`StorageBackend::kb_find_unbounded`]; built-in
    /// backends override it to avoid loading document bodies.
    async fn kb_find_metadata_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<KbMetadataProjection>> {
        Ok(self
            .kb_find_unbounded(filter, sort)
            .await?
            .into_iter()
            .map(|document| KbMetadataProjection {
                document_id: document.document_id,
                category: document.category,
                seat_id: document.seat_id,
                metadata_extra: document.metadata.extra,
            })
            .collect())
    }
    async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64>;
    /// Apply a metadata patch to all matching KB docs.
    async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize>;

    /// Soft delete (graveyard), restore, hard purge.
    async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool>;
    async fn kb_restore(&self, document_id: &str) -> SlcResult<bool>;
    async fn kb_purge(&self, document_id: &str) -> SlcResult<bool>;
    async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>>;
    async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64>;

    // ── Episodic store (HISTORY only — never embedded, never searched) ──
    async fn episodic_insert(&self, doc: &Document) -> SlcResult<()>;
    async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool>;
    async fn episodic_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>>;
    async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64>;
    async fn episodic_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize>;
    /// Hard purge (episodic pipeline consumes sources; no graveyard here).
    async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool>;

    // ── Embeddings (KB docs only) ───────────────────────────────
    async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()>;
    async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>>;
    /// All chunk records for a document.
    async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>>;
    /// Vectors across documents (scope-limited) for semantic search.
    async fn all_embeddings(
        &self,
        scope: EmbeddingScope,
        seat_id: Option<&str>,
    ) -> SlcResult<Vec<EmbeddingRecord>>;
    async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()>;

    /// Replace the embedding chunks of several documents in ONE backend write
    /// (each document's chunk list becomes exactly the record handed in — the
    /// delete + insert pair of `reembed_documents`).
    ///
    /// Default body: a per-document `delete_embeddings` + `insert_embeddings`
    /// pair. Backends that persist their whole embedding map on every call must
    /// override this: a bulk re-embed of N documents would otherwise rewrite
    /// the map 2N times (quadratic — see the stale-embedding refresh in
    /// `reembed_documents`, which is the difference between a bounded sweep and
    /// a process that never finishes on a multi-thousand-document vault).
    async fn replace_embeddings(
        &self,
        replacements: &[(String, EmbeddingRecord)],
    ) -> SlcResult<()> {
        for (document_id, record) in replacements {
            self.delete_embeddings(document_id).await?;
            self.insert_embeddings(std::slice::from_ref(record)).await?;
        }
        Ok(())
    }

    // ── Seats ───────────────────────────────────────────────────
    async fn insert_seat(&self, seat: &Seat) -> SlcResult<()>;
    async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>>;
    async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>>;
    async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool>;
    async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool>;
    /// Set the seat's active-task pointer (working-memory context).
    async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool>;
    /// Set the seat's active-document pointer — any category (the context
    /// anchor included in `update_context`).
    async fn set_seat_active_document(
        &self,
        seat_id: &str,
        document_id: Option<&str>,
    ) -> SlcResult<bool>;
    /// The seat's active document id; falls back to the legacy active-task
    /// pointer for seats activated before the unified field existed.
    async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>>;
    async fn incr_seat_stats(
        &self,
        seat_id: &str,
        tool_name: &str,
        tokens_used: i64,
    ) -> SlcResult<bool>;

    // ── Timers ──────────────────────────────────────────────────
    async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()>;
    async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>>;
    async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>>;
    async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()>;

    /// Atomic, immutable reservation for workflow attempt admission. Unsupported
    /// backends MUST reject this operation; put_record is not an acceptable substitute.
    async fn reserve_task_attempt(
        &self,
        _task_id: &str,
        _transport_key: &str,
        _reservation: &Value,
    ) -> SlcResult<Value> {
        Err(crate::error::SlcError::PermissionDenied(
            "task attempt admission requires an atomic supported backend (SQLite)".into(),
        ))
    }
    async fn get_task_attempt(&self, _attempt_id: &str) -> SlcResult<Option<Value>> {
        Err(crate::error::SlcError::PermissionDenied(
            "task attempt admission requires an atomic supported backend (SQLite)".into(),
        ))
    }
    /// Bind a verified held run and append Started plus its task projection
    /// in one SQLite transaction. Unsupported stores must never admit.
    async fn bind_task_attempt(&self, _attempt_id: &str, _run_id: &str) -> SlcResult<Value> {
        Err(crate::error::SlcError::PermissionDenied(
            "atomic attempt binding requires SQLite".into(),
        ))
    }
    /// Persist a verified Hermes no-run abort receipt atomically, rejecting any
    /// intervening bind or reservation mutation. Never synthesize this from expiry.
    async fn abort_task_attempt(
        &self,
        _attempt_id: &str,
        _expected: &Value,
        _receipt: &Value,
    ) -> SlcResult<Value> {
        Err(crate::error::SlcError::PermissionDenied(
            "atomic attempt abort requires SQLite".into(),
        ))
    }
    async fn list_task_attempts(&self, _task_id: &str) -> SlcResult<Vec<Value>> {
        Err(crate::error::SlcError::PermissionDenied(
            "attempt enumeration requires SQLite".into(),
        ))
    }
    async fn has_task_attempt(&self, _task_id: &str) -> SlcResult<bool> {
        Ok(false) // unsupported stores cannot create attempts in the first place
    }
    /// Whether ANY attempt ever existed for the task, aborted ones included:
    /// an aborted reservation stays in the immutable history. Safe (false) for
    /// stores that cannot hold attempts at all, so ordinary workflow calls on
    /// such backends never fail on enumeration.
    async fn has_task_attempt_history(&self, _task_id: &str) -> SlcResult<bool> {
        Ok(false)
    }
    /// Free-form JSON records (task/project extras, notifications, …).
    async fn put_record(&self, collection: &str, key: &str, value: &Value) -> SlcResult<()>;
    async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<Value>>;
    /// Delete a record if present; returns true if it existed.
    async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool>;
    /// Enumerate all records in a collection as `(key, value)` pairs.
    async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, Value)>>;

    /// Re-read the backing store from disk (multi-process setups: several
    /// services may share one vault/db — refresh picks up foreign changes).
    /// Default: no-op.
    async fn refresh(&self) -> SlcResult<()> {
        Ok(())
    }

    /// Flush deferred persistence before the process exits.
    ///
    /// The vault backend coalesces its Git snapshots into one detached worker
    /// (the write path must never block on Git), so a snapshot can still be in
    /// flight at shutdown. A process that exits on top of that worker cuts the
    /// commit and leaves orphan objects behind in `.git`, so the shutdown path
    /// asks for a bounded flush instead of just dropping the runtime. The
    /// default is a no-op: backends without deferred persistence have nothing
    /// to flush. `wait` bounds the work — implementations must return even
    /// when the pending snapshot cannot be drained in time.
    async fn flush_pending(&self, _wait: std::time::Duration) -> SlcResult<()> {
        Ok(())
    }

    async fn health_check(&self) -> bool;
    async fn close(&self) -> SlcResult<()>;
}

/// Delegation: `Arc<dyn StorageBackend>` is itself a backend — lets the
/// engine share one store across seat/search/compression components.
///
/// Every method that the trait declares WITH a default body must be forwarded
/// here explicitly (the compiler cannot tell the difference, and the engine only
/// ever holds this handle). The guard tests for that invariant live in
/// `storage::delegation_tests` below.
#[async_trait]
impl StorageBackend for Arc<dyn StorageBackend> {
    async fn kb_insert(&self, doc: &Document) -> SlcResult<()> {
        self.as_ref().kb_insert(doc).await
    }
    async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>> {
        self.as_ref().kb_get(document_id).await
    }
    async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool> {
        self.as_ref().kb_update_content(document_id, content).await
    }
    async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool> {
        self.as_ref().kb_upsert(doc).await
    }
    async fn kb_replace(&self, doc: &Document) -> SlcResult<bool> {
        self.as_ref().kb_replace(doc).await
    }
    /// Must be delegated explicitly: this method has a default body, so an
    /// implementation that forgets it silently replaces the backend's batch
    /// write with the default one-`kb_replace`-per-document loop (the engine
    /// only ever holds `Arc<dyn StorageBackend>`).
    async fn kb_replace_many(&self, docs: &[Document]) -> SlcResult<()> {
        self.as_ref().kb_replace_many(docs).await
    }
    /// Must be delegated explicitly (default body): without the forward the
    /// atomic backend rename degrades to the default insert + purge pair.
    async fn kb_rename(&self, old_id: &str, new_id: &str) -> SlcResult<bool> {
        self.as_ref().kb_rename(old_id, new_id).await
    }
    async fn kb_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>> {
        self.as_ref().kb_find(filter, sort, limit).await
    }
    async fn kb_find_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<Document>> {
        self.as_ref().kb_find_unbounded(filter, sort).await
    }
    async fn kb_find_metadata_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<KbMetadataProjection>> {
        self.as_ref().kb_find_metadata_unbounded(filter, sort).await
    }
    async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        self.as_ref().kb_count(filter).await
    }
    async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        self.as_ref().kb_patch_meta(filter, patch).await
    }
    async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool> {
        self.as_ref().kb_soft_delete(document_id).await
    }
    async fn kb_restore(&self, document_id: &str) -> SlcResult<bool> {
        self.as_ref().kb_restore(document_id).await
    }
    async fn kb_purge(&self, document_id: &str) -> SlcResult<bool> {
        self.as_ref().kb_purge(document_id).await
    }
    async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>> {
        self.as_ref().kb_graveyard(days).await
    }
    async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64> {
        self.as_ref().kb_cleanup_graveyard(days).await
    }

    async fn episodic_insert(&self, doc: &Document) -> SlcResult<()> {
        self.as_ref().episodic_insert(doc).await
    }
    async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool> {
        self.as_ref().episodic_upsert(doc).await
    }
    async fn episodic_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>> {
        self.as_ref().episodic_find(filter, sort, limit).await
    }
    async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        self.as_ref().episodic_count(filter).await
    }
    async fn episodic_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        self.as_ref().episodic_patch_meta(filter, patch).await
    }
    async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool> {
        self.as_ref().episodic_purge(document_id).await
    }

    async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()> {
        self.as_ref().insert_embeddings(records).await
    }
    async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>> {
        self.as_ref().get_embedding(document_id).await
    }
    async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>> {
        self.as_ref().get_all_chunks(document_id).await
    }
    async fn all_embeddings(
        &self,
        scope: EmbeddingScope,
        seat_id: Option<&str>,
    ) -> SlcResult<Vec<EmbeddingRecord>> {
        self.as_ref().all_embeddings(scope, seat_id).await
    }
    async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()> {
        self.as_ref().delete_embeddings(document_id).await
    }
    /// Must be delegated explicitly (default body): without the forward a bulk
    /// embedding refresh degrades to the default delete + insert pair per
    /// document and the vault backend rewrites its whole embedding map 2N times.
    async fn replace_embeddings(
        &self,
        replacements: &[(String, EmbeddingRecord)],
    ) -> SlcResult<()> {
        self.as_ref().replace_embeddings(replacements).await
    }

    async fn insert_seat(&self, seat: &Seat) -> SlcResult<()> {
        self.as_ref().insert_seat(seat).await
    }
    async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
        self.as_ref().get_seat(seat_id).await
    }
    async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>> {
        self.as_ref().list_active_seats(limit).await
    }
    async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool> {
        self.as_ref().touch_seat(seat_id).await
    }
    async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool> {
        self.as_ref().set_seat_status(seat_id, status).await
    }
    async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        self.as_ref().set_seat_active_task(seat_id, task_id).await
    }
    async fn set_seat_active_document(
        &self,
        seat_id: &str,
        document_id: Option<&str>,
    ) -> SlcResult<bool> {
        self.as_ref()
            .set_seat_active_document(seat_id, document_id)
            .await
    }
    async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
        self.as_ref().get_seat_active_document(seat_id).await
    }
    async fn incr_seat_stats(
        &self,
        seat_id: &str,
        tool_name: &str,
        tokens_used: i64,
    ) -> SlcResult<bool> {
        self.as_ref()
            .incr_seat_stats(seat_id, tool_name, tokens_used)
            .await
    }

    async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
        self.as_ref().insert_timer(timer).await
    }
    async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>> {
        self.as_ref().get_timer(timer_id).await
    }
    async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
        self.as_ref().active_timers(seat_id).await
    }
    async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()> {
        self.as_ref().set_timer_fired(timer_id, now).await
    }

    async fn put_record(&self, collection: &str, key: &str, value: &Value) -> SlcResult<()> {
        self.as_ref().put_record(collection, key, value).await
    }
    async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<Value>> {
        self.as_ref().get_record(collection, key).await
    }
    async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool> {
        self.as_ref().delete_record(collection, key).await
    }
    async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, Value)>> {
        self.as_ref().list_records(collection).await
    }

    async fn health_check(&self) -> bool {
        self.as_ref().health_check().await
    }

    /// Must be delegated explicitly: this method has a default body (a no-op),
    /// so an implementation that forgets it turns the multi-process sync — the
    /// engine's refresh call and the periodic `SLC_VAULT_REFRESH_SECS` loop that
    /// shares the vault with another service — into a silent no-op.
    async fn refresh(&self) -> SlcResult<()> {
        self.as_ref().refresh().await
    }

    /// Must be delegated explicitly: this method has a default body, so an
    /// implementation that forgets it turns the engine's shutdown flush into a
    /// silent no-op (the engine only ever holds `Arc<dyn StorageBackend>`).
    async fn flush_pending(&self, wait: std::time::Duration) -> SlcResult<()> {
        self.as_ref().flush_pending(wait).await
    }

    async fn close(&self) -> SlcResult<()> {
        self.as_ref().close().await
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    use crate::model::DocMeta;
    use sqlite::SqliteStore;

    /// Models a downstream backend written against the previous trait surface:
    /// every old required method is present, while the two new unbounded methods
    /// are deliberately omitted so their defaults must compile and run.
    struct LegacyBackend(SqliteStore);

    #[async_trait]
    impl StorageBackend for LegacyBackend {
        async fn kb_insert(&self, doc: &Document) -> SlcResult<()> {
            self.0.kb_insert(doc).await
        }
        async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>> {
            self.0.kb_get(document_id).await
        }
        async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool> {
            self.0.kb_update_content(document_id, content).await
        }
        async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool> {
            self.0.kb_upsert(doc).await
        }
        async fn kb_replace(&self, doc: &Document) -> SlcResult<bool> {
            self.0.kb_replace(doc).await
        }
        async fn kb_find(
            &self,
            filter: &DocFilter,
            sort: &DocSort,
            limit: usize,
        ) -> SlcResult<Vec<Document>> {
            self.0.kb_find(filter, sort, limit).await
        }
        async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64> {
            self.0.kb_count(filter).await
        }
        async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
            self.0.kb_patch_meta(filter, patch).await
        }
        async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool> {
            self.0.kb_soft_delete(document_id).await
        }
        async fn kb_restore(&self, document_id: &str) -> SlcResult<bool> {
            self.0.kb_restore(document_id).await
        }
        async fn kb_purge(&self, document_id: &str) -> SlcResult<bool> {
            self.0.kb_purge(document_id).await
        }
        async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>> {
            self.0.kb_graveyard(days).await
        }
        async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64> {
            self.0.kb_cleanup_graveyard(days).await
        }
        async fn episodic_insert(&self, doc: &Document) -> SlcResult<()> {
            self.0.episodic_insert(doc).await
        }
        async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool> {
            self.0.episodic_upsert(doc).await
        }
        async fn episodic_find(
            &self,
            filter: &DocFilter,
            sort: &DocSort,
            limit: usize,
        ) -> SlcResult<Vec<Document>> {
            self.0.episodic_find(filter, sort, limit).await
        }
        async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64> {
            self.0.episodic_count(filter).await
        }
        async fn episodic_patch_meta(
            &self,
            filter: &DocFilter,
            patch: &MetaPatch,
        ) -> SlcResult<usize> {
            self.0.episodic_patch_meta(filter, patch).await
        }
        async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool> {
            self.0.episodic_purge(document_id).await
        }
        async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()> {
            self.0.insert_embeddings(records).await
        }
        async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>> {
            self.0.get_embedding(document_id).await
        }
        async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>> {
            self.0.get_all_chunks(document_id).await
        }
        async fn all_embeddings(
            &self,
            scope: EmbeddingScope,
            seat_id: Option<&str>,
        ) -> SlcResult<Vec<EmbeddingRecord>> {
            self.0.all_embeddings(scope, seat_id).await
        }
        async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()> {
            self.0.delete_embeddings(document_id).await
        }
        async fn insert_seat(&self, seat: &Seat) -> SlcResult<()> {
            self.0.insert_seat(seat).await
        }
        async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
            self.0.get_seat(seat_id).await
        }
        async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>> {
            self.0.list_active_seats(limit).await
        }
        async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool> {
            self.0.touch_seat(seat_id).await
        }
        async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool> {
            self.0.set_seat_status(seat_id, status).await
        }
        async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
            self.0.set_seat_active_task(seat_id, task_id).await
        }
        async fn set_seat_active_document(
            &self,
            seat_id: &str,
            document_id: Option<&str>,
        ) -> SlcResult<bool> {
            self.0.set_seat_active_document(seat_id, document_id).await
        }
        async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
            self.0.get_seat_active_document(seat_id).await
        }
        async fn incr_seat_stats(
            &self,
            seat_id: &str,
            tool_name: &str,
            tokens_used: i64,
        ) -> SlcResult<bool> {
            self.0
                .incr_seat_stats(seat_id, tool_name, tokens_used)
                .await
        }
        async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
            self.0.insert_timer(timer).await
        }
        async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>> {
            self.0.get_timer(timer_id).await
        }
        async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
            self.0.active_timers(seat_id).await
        }
        async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()> {
            self.0.set_timer_fired(timer_id, now).await
        }
        async fn put_record(&self, collection: &str, key: &str, value: &Value) -> SlcResult<()> {
            self.0.put_record(collection, key, value).await
        }
        async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<Value>> {
            self.0.get_record(collection, key).await
        }
        async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool> {
            self.0.delete_record(collection, key).await
        }
        async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, Value)>> {
            self.0.list_records(collection).await
        }
        async fn health_check(&self) -> bool {
            self.0.health_check().await
        }
        async fn close(&self) -> SlcResult<()> {
            self.0.close().await
        }
    }

    #[tokio::test]
    async fn legacy_backend_inherits_unbounded_query_defaults() {
        let backend = LegacyBackend(SqliteStore::in_memory().unwrap());
        let mut metadata = DocMeta::default();
        metadata
            .extra
            .insert("marker".into(), Value::String("kept".into()));
        let document = Document::new(
            "legacy-default-query",
            DocumentCategory::Documentation,
            "body loaded only by the compatibility fallback",
            metadata,
            vec![],
            Some("legacy-seat".into()),
        );
        backend.kb_insert(&document).await.unwrap();

        let documents = backend
            .kb_find_unbounded(&DocFilter::default(), &DocSort::default())
            .await
            .unwrap();
        assert_eq!(documents.len(), 1);
        let projections = backend
            .kb_find_metadata_unbounded(&DocFilter::default(), &DocSort::default())
            .await
            .unwrap();
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].document_id, "legacy-default-query");
        assert_eq!(projections[0].metadata_extra["marker"], "kept");
    }
}

/// Regression guard for the delegating `impl StorageBackend for
/// Arc<dyn StorageBackend>`.
///
/// A trait method WITH a default body is invisible to the compiler: the
/// delegating impl compiles (and behaves) as if the backend's override did not
/// exist, and the engine never sees the difference — it only ever holds
/// `Arc<dyn StorageBackend>`. `flush_pending` (commit c591c45), `kb_replace_many`,
/// `kb_rename`, `refresh` and `replace_embeddings` all shipped in exactly that
/// state; the defaults are valid implementations, just the wrong ones, so no
/// existing test failed.
///
/// These tests therefore (a) build the store handle exactly the way production
/// does — `Arc<dyn StorageBackend>` — and (b) drive it through the real caller
/// (`SlcEngine`) with a recording backend, asserting per method that the
/// *override* ran and that the default body's tell-tale call pattern did not
/// (`kb_replace` for the batch, insert+purge for the rename). When a new
/// method with a default body is added to the trait, add its forward AND its
/// counter here.
#[cfg(test)]
mod delegation_tests {
    use super::*;
    use crate::llm::CpuHashLlm;
    use crate::{SlcConfig, SlcEngine};
    use sqlite::SqliteStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// Invocations of the trait methods that have a **default body** — the only
    /// ones the delegation can silently drop. The remaining fields are not
    /// recorded for their own sake: they are how the default bodies give
    /// themselves away (`kb_replace_many` → a loop of `kb_replace`,
    /// `kb_rename` → `kb_get` + `kb_insert` + `kb_purge`).
    #[derive(Default)]
    struct Reached {
        kb_replace_many: AtomicUsize,
        /// Document count per batch call: one entry when the whole batch went
        /// through in one call, N entries when the default loop ran.
        kb_replace_many_batches: Mutex<Vec<usize>>,
        kb_rename: AtomicUsize,
        kb_find_unbounded: AtomicUsize,
        kb_find_metadata_unbounded: AtomicUsize,
        refresh: AtomicUsize,
        flush_pending: AtomicUsize,
        kb_replace: AtomicUsize,
        kb_insert: AtomicUsize,
        kb_purge: AtomicUsize,
        /// Batched embedding writes (`replace_embeddings` override).
        replace_embeddings: AtomicUsize,
        /// Document count per batch: one entry per chunk.
        replace_embeddings_batches: Mutex<Vec<usize>>,
        /// The default body's per-document tell-tale pair.
        delete_embeddings: AtomicUsize,
        insert_embeddings: AtomicUsize,
    }

    fn bump(field: &AtomicUsize) {
        field.fetch_add(1, Ordering::SeqCst);
    }

    fn count(field: &AtomicUsize) -> usize {
        field.load(Ordering::SeqCst)
    }

    /// Stands in for a backend such as `ObsidianVaultStore`: it overrides the
    /// default-bodied methods with counted ones, so a missed forward shows up as
    /// a zero counter (or as the default body's call pattern) instead of a
    /// silent behaviour change.
    struct RecordingBackend {
        inner: SqliteStore,
        reached: Arc<Reached>,
    }

    impl RecordingBackend {
        /// Returns the backend and the counters it fills. The inner store is
        /// reachable directly so tests can seed data without touching the
        /// counters.
        fn new() -> (Arc<Self>, Arc<Reached>) {
            let reached = Arc::new(Reached::default());
            let backend = Arc::new(RecordingBackend {
                inner: SqliteStore::in_memory().unwrap(),
                reached: reached.clone(),
            });
            (backend, reached)
        }
    }

    #[async_trait]
    impl StorageBackend for RecordingBackend {
        async fn kb_insert(&self, doc: &Document) -> SlcResult<()> {
            bump(&self.reached.kb_insert);
            self.inner.kb_insert(doc).await
        }
        async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>> {
            self.inner.kb_get(document_id).await
        }
        async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool> {
            self.inner.kb_update_content(document_id, content).await
        }
        async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool> {
            self.inner.kb_upsert(doc).await
        }
        async fn kb_replace(&self, doc: &Document) -> SlcResult<bool> {
            bump(&self.reached.kb_replace);
            self.inner.kb_replace(doc).await
        }
        /// Counted override of a default-bodied method.
        async fn kb_replace_many(&self, docs: &[Document]) -> SlcResult<()> {
            bump(&self.reached.kb_replace_many);
            self.reached
                .kb_replace_many_batches
                .lock()
                .unwrap()
                .push(docs.len());
            self.inner.kb_replace_many(docs).await
        }
        /// Counted override of a default-bodied method.
        async fn kb_rename(&self, old_id: &str, new_id: &str) -> SlcResult<bool> {
            bump(&self.reached.kb_rename);
            self.inner.kb_rename(old_id, new_id).await
        }
        async fn kb_find(
            &self,
            filter: &DocFilter,
            sort: &DocSort,
            limit: usize,
        ) -> SlcResult<Vec<Document>> {
            self.inner.kb_find(filter, sort, limit).await
        }
        /// Counted override of a default-bodied method.
        async fn kb_find_unbounded(
            &self,
            filter: &DocFilter,
            sort: &DocSort,
        ) -> SlcResult<Vec<Document>> {
            bump(&self.reached.kb_find_unbounded);
            self.inner.kb_find_unbounded(filter, sort).await
        }
        /// Counted override of a default-bodied method.
        async fn kb_find_metadata_unbounded(
            &self,
            filter: &DocFilter,
            sort: &DocSort,
        ) -> SlcResult<Vec<KbMetadataProjection>> {
            bump(&self.reached.kb_find_metadata_unbounded);
            self.inner.kb_find_metadata_unbounded(filter, sort).await
        }
        async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64> {
            self.inner.kb_count(filter).await
        }
        async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
            self.inner.kb_patch_meta(filter, patch).await
        }
        async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool> {
            self.inner.kb_soft_delete(document_id).await
        }
        async fn kb_restore(&self, document_id: &str) -> SlcResult<bool> {
            self.inner.kb_restore(document_id).await
        }
        async fn kb_purge(&self, document_id: &str) -> SlcResult<bool> {
            bump(&self.reached.kb_purge);
            self.inner.kb_purge(document_id).await
        }
        async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>> {
            self.inner.kb_graveyard(days).await
        }
        async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64> {
            self.inner.kb_cleanup_graveyard(days).await
        }
        async fn episodic_insert(&self, doc: &Document) -> SlcResult<()> {
            self.inner.episodic_insert(doc).await
        }
        async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool> {
            self.inner.episodic_upsert(doc).await
        }
        async fn episodic_find(
            &self,
            filter: &DocFilter,
            sort: &DocSort,
            limit: usize,
        ) -> SlcResult<Vec<Document>> {
            self.inner.episodic_find(filter, sort, limit).await
        }
        async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64> {
            self.inner.episodic_count(filter).await
        }
        async fn episodic_patch_meta(
            &self,
            filter: &DocFilter,
            patch: &MetaPatch,
        ) -> SlcResult<usize> {
            self.inner.episodic_patch_meta(filter, patch).await
        }
        async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool> {
            self.inner.episodic_purge(document_id).await
        }
        async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()> {
            bump(&self.reached.insert_embeddings);
            self.inner.insert_embeddings(records).await
        }
        async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>> {
            self.inner.get_embedding(document_id).await
        }
        async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>> {
            self.inner.get_all_chunks(document_id).await
        }
        async fn all_embeddings(
            &self,
            scope: EmbeddingScope,
            seat_id: Option<&str>,
        ) -> SlcResult<Vec<EmbeddingRecord>> {
            self.inner.all_embeddings(scope, seat_id).await
        }
        async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()> {
            bump(&self.reached.delete_embeddings);
            self.inner.delete_embeddings(document_id).await
        }
        /// Counted override of a default-bodied method.
        async fn replace_embeddings(
            &self,
            replacements: &[(String, EmbeddingRecord)],
        ) -> SlcResult<()> {
            bump(&self.reached.replace_embeddings);
            self.reached
                .replace_embeddings_batches
                .lock()
                .unwrap()
                .push(replacements.len());
            self.inner.replace_embeddings(replacements).await
        }
        async fn insert_seat(&self, seat: &Seat) -> SlcResult<()> {
            self.inner.insert_seat(seat).await
        }
        async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
            self.inner.get_seat(seat_id).await
        }
        async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>> {
            self.inner.list_active_seats(limit).await
        }
        async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool> {
            self.inner.touch_seat(seat_id).await
        }
        async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool> {
            self.inner.set_seat_status(seat_id, status).await
        }
        async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
            self.inner.set_seat_active_task(seat_id, task_id).await
        }
        async fn set_seat_active_document(
            &self,
            seat_id: &str,
            document_id: Option<&str>,
        ) -> SlcResult<bool> {
            self.inner
                .set_seat_active_document(seat_id, document_id)
                .await
        }
        async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
            self.inner.get_seat_active_document(seat_id).await
        }
        async fn incr_seat_stats(
            &self,
            seat_id: &str,
            tool_name: &str,
            tokens_used: i64,
        ) -> SlcResult<bool> {
            self.inner
                .incr_seat_stats(seat_id, tool_name, tokens_used)
                .await
        }
        async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
            self.inner.insert_timer(timer).await
        }
        async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>> {
            self.inner.get_timer(timer_id).await
        }
        async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
            self.inner.active_timers(seat_id).await
        }
        async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()> {
            self.inner.set_timer_fired(timer_id, now).await
        }
        async fn put_record(&self, collection: &str, key: &str, value: &Value) -> SlcResult<()> {
            self.inner.put_record(collection, key, value).await
        }
        async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<Value>> {
            self.inner.get_record(collection, key).await
        }
        async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool> {
            self.inner.delete_record(collection, key).await
        }
        async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, Value)>> {
            self.inner.list_records(collection).await
        }
        /// Counted override of a default-bodied method.
        async fn refresh(&self) -> SlcResult<()> {
            bump(&self.reached.refresh);
            self.inner.refresh().await
        }
        /// Counted override of a default-bodied method.
        async fn flush_pending(&self, wait: std::time::Duration) -> SlcResult<()> {
            bump(&self.reached.flush_pending);
            self.inner.flush_pending(wait).await
        }
        async fn health_check(&self) -> bool {
            self.inner.health_check().await
        }
        async fn close(&self) -> SlcResult<()> {
            self.inner.close().await
        }
    }

    fn doc(id: &str, auto_load: Vec<String>) -> Document {
        let mut document = Document::new(
            id,
            DocumentCategory::Documentation,
            format!("body of {id}"),
            crate::model::DocMeta::default(),
            vec![],
            None,
        );
        document.auto_load = auto_load;
        document
    }

    /// The whole contract in one place: every method of the trait that has a
    /// default body must be forwarded by the `Arc<dyn StorageBackend>` impl, or
    /// its override is unreachable through the handle the engine holds.
    #[tokio::test]
    async fn every_default_body_override_is_reached_through_the_dyn_handle() {
        let (backend, reached) = RecordingBackend::new();
        backend
            .inner
            .kb_insert(&doc("dyn-a", vec![]))
            .await
            .unwrap();
        // The exact handle shape `SlcEngine` stores.
        let dynamic: Arc<dyn StorageBackend> = backend.clone();

        dynamic
            .kb_replace_many(&[doc("dyn-b", vec![]), doc("dyn-c", vec![])])
            .await
            .unwrap();
        dynamic.kb_rename("dyn-a", "dyn-a-renamed").await.unwrap();
        dynamic.refresh().await.unwrap();
        dynamic
            .flush_pending(std::time::Duration::from_millis(1))
            .await
            .unwrap();
        dynamic
            .kb_find_unbounded(&DocFilter::default(), &DocSort::default())
            .await
            .unwrap();
        dynamic
            .kb_find_metadata_unbounded(&DocFilter::default(), &DocSort::default())
            .await
            .unwrap();
        dynamic
            .replace_embeddings(&[(
                "dyn-emb".to_string(),
                EmbeddingRecord {
                    document_id: "dyn-emb".into(),
                    chunk_index: 0,
                    chunk_total: 1,
                    embedding: vec![0.5, 0.25],
                    embedding_model: "test".into(),
                    embedding_dimension: 2,
                    generated_at: Utc::now(),
                    scope: EmbeddingScope::Public,
                    seat_id: None,
                },
            )])
            .await
            .unwrap();

        assert_eq!(
            count(&reached.kb_replace_many),
            1,
            "kb_replace_many must reach the backend's batch override through Arc"
        );
        assert_eq!(
            *reached.kb_replace_many_batches.lock().unwrap(),
            vec![2],
            "the batch must arrive as ONE call with both documents"
        );
        assert_eq!(
            count(&reached.kb_replace),
            0,
            "the default batch body (a loop of kb_replace) must not run"
        );
        assert_eq!(
            count(&reached.kb_rename),
            1,
            "kb_rename must reach the backend's atomic rename through Arc"
        );
        assert_eq!(
            count(&reached.kb_insert) + count(&reached.kb_purge),
            0,
            "the default rename body (insert + purge) must not run"
        );
        assert_eq!(
            count(&reached.refresh),
            1,
            "refresh must reach the backend's multi-process resync through Arc"
        );
        assert_eq!(
            count(&reached.flush_pending),
            1,
            "flush_pending must reach the backend's bounded flush through Arc"
        );
        assert_eq!(count(&reached.kb_find_unbounded), 1);
        assert_eq!(count(&reached.kb_find_metadata_unbounded), 1);
        assert_eq!(
            count(&reached.replace_embeddings),
            1,
            "replace_embeddings must reach the backend's batched override through Arc"
        );
        assert_eq!(
            *reached.replace_embeddings_batches.lock().unwrap(),
            vec![1],
            "the batch must arrive as ONE call"
        );
        assert_eq!(
            count(&reached.delete_embeddings) + count(&reached.insert_embeddings),
            0,
            "the default replace body (delete + insert per document) must not run"
        );
    }

    /// Regression: the stale-embedding refresh (`reembed_documents`, driven by
    /// the CLI `search` and `reindex-embeddings`) replaced each document with a
    /// `delete_embeddings` + `insert_embeddings` pair. The vault backend
    /// re-serializes its ENTIRE embedding map in both calls, so a bulk refresh
    /// rewrote an ~80 MB `embeddings.json` 2N times — quadratic, and on the
    /// roy1 vault (10 313 documents) the process burned one core indefinitely
    /// without finishing. One write per chunk, no per-document pair.
    #[tokio::test]
    async fn reembed_documents_writes_one_batch_per_chunk() {
        let (backend, reached) = RecordingBackend::new();
        let ids: Vec<String> = (0..40).map(|i| format!("bulk-{i:02}")).collect();
        for id in &ids {
            backend.inner.kb_insert(&doc(id, vec![])).await.unwrap();
        }
        let docs: Vec<Document> = ids.iter().map(|id| doc(id, vec![])).collect();

        // The exact handle shape `SlcEngine` stores.
        let dynamic: Arc<dyn StorageBackend> = backend.clone();
        let done = crate::reembed_documents(&*dynamic, &CpuHashLlm, &docs).await;

        assert_eq!(done, 40, "every document in both chunks must be rebuilt");
        assert_eq!(
            count(&reached.replace_embeddings),
            2,
            "one batch write per chunk of 32 (32 + 8)"
        );
        assert_eq!(
            *reached.replace_embeddings_batches.lock().unwrap(),
            vec![32, 8],
            "the chunk sizes must arrive in the batch"
        );
        assert_eq!(
            count(&reached.delete_embeddings),
            0,
            "no per-document delete may run under the batch path"
        );
        assert_eq!(
            count(&reached.insert_embeddings),
            0,
            "no per-document insert may run under the batch path"
        );
        // The embeddings really landed (replace = old chunks dropped, one kept).
        for id in &ids {
            assert_eq!(
                dynamic.get_all_chunks(id).await.unwrap().len(),
                1,
                "{id}: exactly the rebuilt record must remain"
            );
        }
    }

    /// The engine's own caller paths — the construction production uses
    /// (`SlcEngine::with(Arc<dyn StorageBackend>, …)`), a cascade that has to be
    /// applied as a batch, and the rename that has to stay atomic.
    #[tokio::test]
    async fn engine_rename_batches_the_cascade_and_uses_the_dyn_store_rename() {
        let (backend, reached) = RecordingBackend::new();
        backend
            .inner
            .kb_insert(&doc("dyn-target", vec![]))
            .await
            .unwrap();
        // A second document references the renamed one: the cascade has one
        // document to fix, so the engine must hand it over as a batch.
        backend
            .inner
            .kb_insert(&doc("dyn-referrer", vec!["dyn-target".into()]))
            .await
            .unwrap();

        let dynamic: Arc<dyn StorageBackend> = backend.clone();
        let engine = SlcEngine::with(dynamic, Arc::new(CpuHashLlm), SlcConfig::default());

        let report = engine
            .rename_document("dyn-seat", "dyn-target", "dyn-target-renamed", None)
            .await
            .unwrap();
        assert_eq!(report.links_fixed, 1, "{report:?}");
        assert_eq!(report.new_id, "dyn-target-renamed");

        assert_eq!(
            count(&reached.kb_replace_many),
            1,
            "the cascading fixes must go through the batch override"
        );
        assert_eq!(
            *reached.kb_replace_many_batches.lock().unwrap(),
            vec![1],
            "one batch call carrying the single changed document"
        );
        assert_eq!(
            count(&reached.kb_replace),
            0,
            "the engine's cascade must not degrade into per-document replaces"
        );
        assert_eq!(
            count(&reached.kb_rename),
            1,
            "the rename itself must reach the backend's atomic override"
        );
        assert_eq!(
            count(&reached.kb_insert) + count(&reached.kb_purge),
            0,
            "the default insert + purge rename must not run under the engine"
        );

        // Functional end state: the document moved and the referrer follows it.
        let referrer = backend.inner.kb_get("dyn-referrer").await.unwrap().unwrap();
        assert_eq!(referrer.auto_load, vec!["dyn-target-renamed".to_string()]);
        assert!(
            backend
                .inner
                .kb_get("dyn-target-renamed")
                .await
                .unwrap()
                .is_some()
        );
    }

    /// `SlcEngine::refresh` and the periodic `SLC_VAULT_REFRESH_SECS` loop
    /// (`SlcEngine::start_background`) both call `refresh` on this same Arc
    /// handle: a missed forward makes the multi-process sync a silent no-op.
    #[tokio::test]
    async fn engine_refresh_reaches_the_store_through_the_dyn_handle() {
        let (backend, reached) = RecordingBackend::new();
        let dynamic: Arc<dyn StorageBackend> = backend.clone();
        let engine = SlcEngine::with(dynamic, Arc::new(CpuHashLlm), SlcConfig::default());

        engine.refresh().await.unwrap();

        assert_eq!(
            count(&reached.refresh),
            1,
            "the engine's refresh must reach the backend's resync through Arc"
        );
    }

    /// The engine is not the only caller: the sqlite backend's comment on the
    /// rename path ("the sqlite move is done in kb_rename") relies on the
    /// override being reachable, and the embedding rows are the observable
    /// difference — the default insert + purge leaves them behind under the old
    /// id (sqlite's `kb_purge` only deletes the document row).
    #[tokio::test]
    async fn dyn_rename_moves_the_sqlite_document_and_its_embeddings() {
        let store = Arc::new(SqliteStore::in_memory().unwrap());
        store.kb_insert(&doc("sqlite-old", vec![])).await.unwrap();
        for chunk in 0..3i64 {
            store
                .insert_embeddings(&[EmbeddingRecord {
                    document_id: "sqlite-old".into(),
                    chunk_index: chunk,
                    chunk_total: 3,
                    embedding: vec![chunk as f32, 1.0],
                    embedding_model: "test".into(),
                    embedding_dimension: 2,
                    generated_at: Utc::now(),
                    scope: EmbeddingScope::Public,
                    seat_id: None,
                }])
                .await
                .unwrap();
        }
        let dynamic: Arc<dyn StorageBackend> = store.clone();

        assert!(dynamic.kb_rename("sqlite-old", "sqlite-new").await.unwrap());

        assert!(
            dynamic.kb_get("sqlite-new").await.unwrap().is_some(),
            "the document must exist under the new id"
        );
        assert!(
            dynamic.kb_get("sqlite-old").await.unwrap().is_none(),
            "the document must be gone under the old id"
        );
        assert_eq!(
            dynamic.get_all_chunks("sqlite-new").await.unwrap().len(),
            3,
            "the rename must move the embedding rows with the document"
        );
        assert!(
            dynamic
                .get_all_chunks("sqlite-old")
                .await
                .unwrap()
                .is_empty(),
            "no embedding row may be orphaned under the old id"
        );
    }
}
