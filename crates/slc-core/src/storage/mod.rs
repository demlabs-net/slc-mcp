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
