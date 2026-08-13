//! Embedded SQLite backend for [`StorageBackend`].
//!
//! Two physical tables (`documents` for the KB, `episodic` for HISTORY) so
//! the "history is not RAG'd" invariant holds at the schema level. All ops
//! run on a `spawn_blocking` worker (rusqlite is sync; the trait is async).

use super::{DocFilter, DocSort, MetaPatch, SortField, SortDir, StorageBackend};
use crate::error::{SlcError, SlcResult};
use crate::model::{
    content_hash, Document, DocumentCategory, EmbeddingRecord, EmbeddingScope,
    PersistedTimer, Seat, SeatStatus, TimerType, UsageStats,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS documents (
    document_id   TEXT PRIMARY KEY,
    category      TEXT NOT NULL,
    folder        TEXT NOT NULL DEFAULT '',
    content       TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    metadata      TEXT NOT NULL,          -- JSON DocMeta
    tags          TEXT NOT NULL,          -- JSON array
    auto_load     TEXT NOT NULL,          -- JSON array
    refs          TEXT NOT NULL,          -- JSON array (references is a SQL keyword)
    seat_id       TEXT,
    created_at    TEXT NOT NULL,          -- RFC3339 UTC
    updated_at    TEXT NOT NULL,
    version       INTEGER NOT NULL DEFAULT 1,
    deleted_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_documents_category ON documents(category);
CREATE INDEX IF NOT EXISTS idx_documents_seat ON documents(seat_id);

CREATE TABLE IF NOT EXISTS episodic (
    document_id   TEXT PRIMARY KEY,
    category      TEXT NOT NULL,
    folder        TEXT NOT NULL DEFAULT '',
    content       TEXT NOT NULL,
    content_hash  TEXT NOT NULL,
    metadata      TEXT NOT NULL,
    tags          TEXT NOT NULL,
    auto_load     TEXT NOT NULL,
    refs          TEXT NOT NULL,
    seat_id       TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    version       INTEGER NOT NULL DEFAULT 1,
    deleted_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_episodic_seat ON episodic(seat_id);
CREATE INDEX IF NOT EXISTS idx_episodic_level ON episodic(metadata);

CREATE TABLE IF NOT EXISTS embeddings (
    document_id        TEXT NOT NULL,
    chunk_index        INTEGER NOT NULL,
    chunk_total        INTEGER NOT NULL,
    embedding          TEXT NOT NULL,     -- JSON array of f32
    embedding_model    TEXT NOT NULL,
    embedding_dimension INTEGER NOT NULL,
    generated_at       TEXT NOT NULL,
    scope              TEXT NOT NULL,     -- "public" | "private"
    seat_id            TEXT,
    PRIMARY KEY (document_id, chunk_index)
);

CREATE TABLE IF NOT EXISTS seats (
    seat_id        TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    status         TEXT NOT NULL,
    created_at     TEXT NOT NULL,
    last_accessed  TEXT NOT NULL,
    expires_at     TEXT,
    metadata       TEXT NOT NULL,
    active_task_id TEXT,
    context        TEXT NOT NULL,
    usage_stats    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS timers (
    timer_id         TEXT PRIMARY KEY,
    seat_id          TEXT NOT NULL,
    timer_type       TEXT NOT NULL,
    interval_seconds INTEGER,
    last_fired_at    TEXT,
    next_fire_at     TEXT NOT NULL,
    is_active        INTEGER NOT NULL DEFAULT 1,
    metadata         TEXT NOT NULL,
    created_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_timers_seat ON timers(seat_id);

CREATE TABLE IF NOT EXISTS records (
    collection TEXT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,             -- JSON
    PRIMARY KEY (collection, key)
);
"#;

/// Embedded SQLite store. `conn` is `Arc<Mutex<Connection>>` — every trait
/// method hops to a blocking thread; the mutex serializes writers.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open (or create) the database at `path`; `:memory:` is allowed.
    pub fn open(path: impl AsRef<Path>) -> SlcResult<Self> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    /// In-memory store (tests / embedded use).
    pub fn in_memory() -> SlcResult<Self> {
        Self::open(":memory:")
    }

    async fn blocking<T, F>(&self, f: F) -> SlcResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> SlcResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&conn.lock().unwrap()))
            .await
            .map_err(|e| SlcError::Storage(format!("blocking task failed: {e}")))?
    }
}

// ─────────────────────────────── row <-> model ───────────────────────────────

fn doc_to_row(doc: &Document) -> SlcResult<(String, String, String, String, String, String, String, String, String, Option<String>, String, String, i64, Option<String>)> {
    Ok((
        doc.document_id.clone(),
        doc.category.as_str().to_string(),
        doc.folder.clone().unwrap_or_else(|| doc.default_folder()),
        doc.content.clone(),
        doc.content_hash.clone(),
        serde_json::to_string(&doc.metadata)?,
        serde_json::to_string(&doc.tags)?,
        serde_json::to_string(&doc.auto_load)?,
        serde_json::to_string(&doc.references)?,
        doc.seat_id.clone(),
        doc.created_at.to_rfc3339(),
        doc.updated_at.to_rfc3339(),
        doc.version,
        doc.deleted_at.map(|d| d.to_rfc3339()),
    ))
}

fn row_to_doc(row: &Row) -> rusqlite::Result<Document> {
    let category: String = row.get(1)?;
    let metadata: String = row.get(5)?;
    let tags: String = row.get(6)?;
    let auto_load: String = row.get(7)?;
    let references: String = row.get(8)?;
    let deleted_at: Option<String> = row.get(13)?;
    Ok(Document {
        document_id: row.get(0)?,
        category: DocumentCategory::parse(&category).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, format!("bad category {category}").into())
        })?,
        folder: Some(row.get(2)?),
        content: row.get(3)?,
        content_hash: row.get(4)?,
        metadata: serde_json::from_str(&metadata).unwrap_or_default(),
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        auto_load: serde_json::from_str(&auto_load).unwrap_or_default(),
        references: serde_json::from_str(&references).unwrap_or_default(),
        seat_id: row.get(9)?,
        created_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(10)?)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        updated_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(11)?)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        version: row.get(12)?,
        deleted_at: deleted_at
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)),
    })
}

// ─────────────────────────────── filter -> SQL ───────────────────────────────

/// Build `WHERE` clauses + params for the shared doc tables.
fn build_where(f: &DocFilter) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    if let Some(cat) = f.category {
        clauses.push("category = ?".to_string());
        params.push(cat.as_str().to_string());
    }
    if let Some(seat) = &f.seat_id {
        clauses.push("seat_id = ?".to_string());
        params.push(seat.clone());
    }
    if let Some(vis) = &f.visible_to {
        clauses.push("(seat_id IS NULL OR seat_id = ?)".to_string());
        params.push(vis.clone());
    }
    if let Some(level) = f.doc_level {
        clauses.push("json_extract(metadata, '$.doc_level') = ?".to_string());
        params.push(level.as_str().to_string());
    }
    if let Some(dt) = &f.doc_type {
        clauses.push("json_extract(metadata, '$.doc_type') = ?".to_string());
        params.push(dt.clone());
    }
    if let Some(archived) = f.archived {
        if archived {
            clauses.push("json_extract(metadata, '$.archived') = 1".to_string());
        } else {
            clauses.push("(json_extract(metadata, '$.archived') IS NULL OR json_extract(metadata, '$.archived') = 0)".to_string());
        }
    }
    if f.not_consolidated {
        clauses.push("(json_extract(metadata, '$.consolidated') IS NULL OR json_extract(metadata, '$.consolidated') = 0)".to_string());
    }
    match f.has_compression_batch {
        Some(true) => clauses.push("json_extract(metadata, '$.compression_batch_id') IS NOT NULL".to_string()),
        Some(false) => clauses.push("json_extract(metadata, '$.compression_batch_id') IS NULL".to_string()),
        None => {}
    }
    if !f.tags_any.is_empty() {
        // tags stored as JSON array — match if ANY listed tag is a member.
        let ors: Vec<String> = f
            .tags_any
            .iter()
            .map(|t| format!("EXISTS (SELECT 1 FROM json_each(tags) WHERE json_each.value = ?{})", params.len() + 1))
            .collect();
        clauses.push(format!("({})", ors.join(" OR ")));
        params.extend(f.tags_any.iter().cloned());
    }
    if !f.tags_all.is_empty() {
        let ands: Vec<String> = f
            .tags_all
            .iter()
            .map(|t| format!("EXISTS (SELECT 1 FROM json_each(tags) WHERE json_each.value = ?{})", params.len() + 1))
            .collect();
        clauses.push(format!("({})", ands.join(" AND ")));
        params.extend(f.tags_all.iter().cloned());
    }
    if f.deleted {
        clauses.push("deleted_at IS NOT NULL".to_string());
    } else {
        clauses.push("deleted_at IS NULL".to_string());
    }
    if let Some(since) = f.since {
        clauses.push("updated_at >= ?".to_string());
        params.push(since.to_rfc3339());
    }
    if let Some(until) = f.until {
        clauses.push("updated_at <= ?".to_string());
        params.push(until.to_rfc3339());
    }

    let where_sql = if clauses.is_empty() { String::new() } else { format!("WHERE {}", clauses.join(" AND ")) };
    (where_sql, params)
}

fn order_by(sort: &DocSort) -> &'static str {
    match sort.field {
        SortField::CreatedAt => "created_at",
        SortField::UpdatedAt => "updated_at",
    }
}
fn order_dir(sort: &DocSort) -> &'static str {
    match sort.dir {
        SortDir::Asc => "ASC",
        SortDir::Desc => "DESC",
    }
}

fn kv_params(params: &[String]) -> Vec<&dyn rusqlite::types::ToSql> {
    params.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect()
}

// ─────────────────────────────── impl StorageBackend ───────────────────────────────

#[async_trait]
impl StorageBackend for SqliteStore {
    async fn kb_insert(&self, doc: &Document) -> SlcResult<()> {
        if !doc.category.is_kb() {
            return Err(SlcError::Storage(format!(
                "history docs go to the episodic store, not the KB: {}",
                doc.document_id
            )));
        }
        let row = doc_to_row(doc)?;
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO documents (document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11, row.12, row.13],
            )?;
            Ok(())
        })
        .await
    }

    async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            let row = conn
                .query_row(
                    "SELECT document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at
                     FROM documents WHERE document_id = ?1 AND deleted_at IS NULL",
                    params![id],
                    row_to_doc,
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool> {
        let (id, content, hash) = (document_id.to_string(), content.to_string(), content_hash(content));
        self.blocking(move |conn| {
            let n = conn.execute(
                "UPDATE documents SET content = ?2, content_hash = ?3, version = version + 1, updated_at = ?4
                 WHERE document_id = ?1 AND deleted_at IS NULL",
                params![id, content, hash, Utc::now().to_rfc3339()],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool> {
        if self.kb_update_content(&doc.document_id, &doc.content).await? {
            return Ok(true);
        }
        self.kb_insert(doc).await?;
        Ok(false)
    }

    async fn kb_find(&self, filter: &DocFilter, sort: &DocSort, limit: usize) -> SlcResult<Vec<Document>> {
        let f = filter.clone();
        let sort = *sort;
        self.blocking(move |conn| {
            let (where_sql, params) = build_where(&f);
            let sql = format!(
                "SELECT document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at
                 FROM documents {where_sql} ORDER BY {} {} LIMIT ?{}",
                order_by(&sort),
                order_dir(&sort),
                params.len() + 1
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut ps = kv_params(&params);
            let limit_i = limit as i64;
            ps.push(&limit_i as &dyn rusqlite::types::ToSql);
            let rows = stmt.query_map(ps.as_slice(), row_to_doc)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        let f = filter.clone();
        self.blocking(move |conn| {
            let (where_sql, params) = build_where(&f);
            let sql = format!("SELECT COUNT(*) FROM documents {where_sql}");
            let n: i64 = conn.query_row(&sql, kv_params(&params).as_slice(), |r| r.get(0))?;
            Ok(n as u64)
        })
        .await
    }

    async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        // Read-modify-write: load matching docs, apply the patch, persist.
        let f = filter.clone();
        let patch = patch.clone();
        let docs = self.kb_find(&f, &DocSort::default(), usize::MAX).await?;
        let mut changed = 0;
        for doc in docs {
            let mut m = doc.metadata.clone();
            if let Some(a) = patch.set_archived {
                m.archived = Some(a);
            }
            if let Some(c) = patch.set_consolidated {
                m.consolidated = Some(c);
            }
            if let Some(b) = &patch.set_compression_batch_id {
                m.compression_batch_id = Some(b.clone());
            }
            let meta_json = serde_json::to_string(&m)?;
            let id = doc.document_id.clone();
            self.blocking(move |conn| {
                conn.execute(
                    "UPDATE documents SET metadata = ?2, updated_at = ?3 WHERE document_id = ?1",
                    params![id, meta_json, Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await?;
            changed += 1;
        }
        Ok(changed)
    }

    async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            let n = conn.execute(
                "UPDATE documents SET deleted_at = ?2, version = version + 1 WHERE document_id = ?1 AND deleted_at IS NULL",
                params![id, Utc::now().to_rfc3339()],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn kb_restore(&self, document_id: &str) -> SlcResult<bool> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            let n = conn.execute(
                "UPDATE documents SET deleted_at = NULL WHERE document_id = ?1",
                params![id],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn kb_purge(&self, document_id: &str) -> SlcResult<bool> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            let n = conn.execute("DELETE FROM documents WHERE document_id = ?1", params![id])?;
            Ok(n > 0)
        })
        .await
    }

    async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>> {
        let cutoff = days.map(|d| (Utc::now() - chrono::Duration::days(d)).to_rfc3339());
        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at
                 FROM documents WHERE deleted_at IS NOT NULL AND (?1 IS NULL OR deleted_at <= ?1)
                 ORDER BY deleted_at DESC",
            )?;
            let rows = stmt.query_map(params![cutoff], row_to_doc)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64> {
        let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        self.blocking(move |conn| {
            let n = conn.execute("DELETE FROM documents WHERE deleted_at IS NOT NULL AND deleted_at <= ?1", params![cutoff])?;
            Ok(n as u64)
        })
        .await
    }

    // ── episodic ────────────────────────────────────────────────

    async fn episodic_insert(&self, doc: &Document) -> SlcResult<()> {
        if doc.category != DocumentCategory::History {
            return Err(SlcError::Storage(format!(
                "only history docs go to the episodic store: {} ({:?})",
                doc.document_id, doc.category
            )));
        }
        if doc.seat_id.is_none() {
            return Err(SlcError::Storage("episodic docs must be seat-scoped".into()));
        }
        let row = doc_to_row(doc)?;
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO episodic (document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11, row.12, row.13],
            )?;
            Ok(())
        })
        .await
    }

    async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool> {
        // Upsert by unique NAME id (e.g. episodic_l4_{seat}).
        let row = doc_to_row(doc)?;
        let existing = self
            .episodic_count(&DocFilter { seat_id: doc.seat_id.clone(), ..Default::default() })
            .await;
        let _ = existing;
        self.blocking(move |conn| {
            let n = conn.execute(
                "UPDATE episodic SET folder = ?3, content = ?4, content_hash = ?5, metadata = ?6, tags = ?7, version = version + 1, updated_at = ?12
                 WHERE document_id = ?1",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11, row.12, row.13],
            )?;
            if n > 0 {
                return Ok(true);
            }
            conn.execute(
                "INSERT INTO episodic (document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11, row.12, row.13],
            )?;
            Ok(false)
        })
        .await
    }

    async fn episodic_find(&self, filter: &DocFilter, sort: &DocSort, limit: usize) -> SlcResult<Vec<Document>> {
        let f = filter.clone();
        let sort = *sort;
        self.blocking(move |conn| {
            let (where_sql, params) = build_where(&f);
            let sql = format!(
                "SELECT document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at
                 FROM episodic {where_sql} ORDER BY {} {} LIMIT ?{}",
                order_by(&sort),
                order_dir(&sort),
                params.len() + 1
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut ps = kv_params(&params);
            let limit_i = limit as i64;
            ps.push(&limit_i as &dyn rusqlite::types::ToSql);
            let rows = stmt.query_map(ps.as_slice(), row_to_doc)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        let f = filter.clone();
        self.blocking(move |conn| {
            let (where_sql, params) = build_where(&f);
            let sql = format!("SELECT COUNT(*) FROM episodic {where_sql}");
            let n: i64 = conn.query_row(&sql, kv_params(&params).as_slice(), |r| r.get(0))?;
            Ok(n as u64)
        })
        .await
    }

    async fn episodic_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        let f = filter.clone();
        let patch = patch.clone();
        let docs = self.episodic_find(&f, &DocSort::default(), usize::MAX).await?;
        let mut changed = 0;
        for doc in docs {
            let mut m = doc.metadata.clone();
            if let Some(a) = patch.set_archived {
                m.archived = Some(a);
            }
            if let Some(c) = patch.set_consolidated {
                m.consolidated = Some(c);
            }
            if let Some(b) = &patch.set_compression_batch_id {
                m.compression_batch_id = Some(b.clone());
            }
            let meta_json = serde_json::to_string(&m)?;
            let id = doc.document_id.clone();
            self.blocking(move |conn| {
                conn.execute(
                    "UPDATE episodic SET metadata = ?2, updated_at = ?3 WHERE document_id = ?1",
                    params![id, meta_json, Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await?;
            changed += 1;
        }
        Ok(changed)
    }

    async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            let n = conn.execute("DELETE FROM episodic WHERE document_id = ?1", params![id])?;
            Ok(n > 0)
        })
        .await
    }

    // ── embeddings ──────────────────────────────────────────────

    async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()> {
        let rows: Vec<(String, i64, i64, String, String, i64, String, String, Option<String>)> = records
            .iter()
            .map(|r| {
                Ok((
                    r.document_id.clone(),
                    r.chunk_index,
                    r.chunk_total,
                    serde_json::to_string(&r.embedding)?,
                    r.embedding_model.clone(),
                    r.embedding_dimension as i64,
                    r.generated_at.to_rfc3339(),
                    match r.scope {
                        EmbeddingScope::Public => "public",
                        EmbeddingScope::Private => "private",
                    }
                    .to_string(),
                    r.seat_id.clone(),
                ))
            })
            .collect::<SlcResult<Vec<_>>>()?;
        self.blocking(move |conn| {
            let tx = conn.unchecked_transaction()?;
            for r in &rows {
                tx.execute(
                    "INSERT OR REPLACE INTO embeddings (document_id, chunk_index, chunk_total, embedding, embedding_model, embedding_dimension, generated_at, scope, seat_id)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            let emb: Option<String> = conn
                .query_row(
                    "SELECT embedding FROM embeddings WHERE document_id = ?1 AND chunk_index = 0",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(emb.map(|e| serde_json::from_str(&e).unwrap_or_default()))
        })
        .await
    }

    async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT document_id, chunk_index, chunk_total, embedding, embedding_model, embedding_dimension, generated_at, scope, seat_id
                 FROM embeddings WHERE document_id = ?1 ORDER BY chunk_index",
            )?;
            let rows = stmt.query_map(params![id], row_to_embedding)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn all_embeddings(&self, scope: EmbeddingScope, seat_id: Option<&str>) -> SlcResult<Vec<EmbeddingRecord>> {
        let scope_str = match scope {
            EmbeddingScope::Public => "public",
            EmbeddingScope::Private => "private",
        }
        .to_string();
        let seat = seat_id.map(|s| s.to_string());
        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT document_id, chunk_index, chunk_total, embedding, embedding_model, embedding_dimension, generated_at, scope, seat_id
                 FROM embeddings WHERE scope = ?1 AND (?2 IS NULL OR seat_id = ?2) ORDER BY document_id, chunk_index",
            )?;
            let rows = stmt.query_map(params![scope_str, seat], row_to_embedding)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()> {
        let id = document_id.to_string();
        self.blocking(move |conn| {
            conn.execute("DELETE FROM embeddings WHERE document_id = ?1", params![id])?;
            Ok(())
        })
        .await
    }

    // ── seats ───────────────────────────────────────────────────

    async fn insert_seat(&self, seat: &Seat) -> SlcResult<()> {
        let (seat_id, name, status, created_at, last_accessed, expires_at, metadata, active_task_id, context, usage_stats) = (
            seat.seat_id.clone(),
            seat.name.clone(),
            seat.status.as_str().to_string(),
            seat.created_at.to_rfc3339(),
            seat.last_accessed.to_rfc3339(),
            seat.expires_at.map(|d| d.to_rfc3339()),
            serde_json::to_string(&seat.metadata)?,
            seat.active_task_id.clone(),
            serde_json::to_string(&seat.context)?,
            serde_json::to_string(&seat.usage_stats)?,
        );
        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO seats (seat_id, name, status, created_at, last_accessed, expires_at, metadata, active_task_id, context, usage_stats)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![seat_id, name, status, created_at, last_accessed, expires_at, metadata, active_task_id, context, usage_stats],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
        let id = seat_id.to_string();
        self.blocking(move |conn| {
            let row = conn.query_row("SELECT * FROM seats WHERE seat_id = ?1", params![id], row_to_seat).optional()?;
            Ok(row)
        })
        .await
    }

    async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>> {
        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT * FROM seats WHERE status = 'active' ORDER BY last_accessed DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], row_to_seat)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool> {
        let id = seat_id.to_string();
        self.blocking(move |conn| {
            let n = conn.execute(
                "UPDATE seats SET last_accessed = ?2 WHERE seat_id = ?1",
                params![id, Utc::now().to_rfc3339()],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool> {
        let id = seat_id.to_string();
        let status = status.as_str().to_string();
        self.blocking(move |conn| {
            let n = conn.execute("UPDATE seats SET status = ?2 WHERE seat_id = ?1", params![id, status])?;
            Ok(n > 0)
        })
        .await
    }

    async fn incr_seat_stats(&self, seat_id: &str, tool_name: &str, tokens_used: i64) -> SlcResult<bool> {
        let id = seat_id.to_string();
        let tool = tool_name.to_string();
        self.blocking(move |conn| {
            let stats: Option<String> = conn
                .query_row("SELECT usage_stats FROM seats WHERE seat_id = ?1", params![id], |r| r.get(0))
                .optional()?;
            let mut stats: UsageStats = stats.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            stats.total_requests += 1;
            stats.total_tokens += tokens_used;
            let count = stats
                .tools_used
                .get(&tool)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                + 1;
            stats.tools_used.insert(tool, serde_json::Value::from(count));
            let json = serde_json::to_string(&stats)?;
            let n = conn.execute(
                "UPDATE seats SET usage_stats = ?2, last_accessed = ?3 WHERE seat_id = ?1",
                params![id, json, Utc::now().to_rfc3339()],
            )?;
            Ok(n > 0)
        })
        .await
    }

    // ── timers ──────────────────────────────────────────────────

    async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
        let (timer_id, seat_id, timer_type, interval_seconds, last_fired_at, next_fire_at, is_active, metadata, created_at) = (
            timer.timer_id.clone(),
            timer.seat_id.clone(),
            timer.timer_type.as_str().to_string(),
            timer.interval_seconds,
            timer.last_fired_at.map(|d| d.to_rfc3339()),
            timer.next_fire_at.to_rfc3339(),
            timer.is_active as i64,
            serde_json::to_string(&timer.metadata)?,
            timer.created_at.to_rfc3339(),
        );
        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO timers (timer_id, seat_id, timer_type, interval_seconds, last_fired_at, next_fire_at, is_active, metadata, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![timer_id, seat_id, timer_type, interval_seconds, last_fired_at, next_fire_at, is_active, metadata, created_at],
            )?;
            Ok(())
        })
        .await
    }

    async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
        let seat = seat_id.map(|s| s.to_string());
        self.blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT * FROM timers WHERE is_active = 1 AND (?1 IS NULL OR seat_id = ?1) ORDER BY next_fire_at",
            )?;
            let rows = stmt.query_map(params![seat], row_to_timer)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()> {
        let id = timer_id.to_string();
        let now = now.to_rfc3339();
        self.blocking(move |conn| {
            conn.execute(
                "UPDATE timers SET last_fired_at = ?2 WHERE timer_id = ?1",
                params![id, now],
            )?;
            Ok(())
        })
        .await
    }

    // ── records ─────────────────────────────────────────────────

    async fn put_record(&self, collection: &str, key: &str, value: &Value) -> SlcResult<()> {
        let (c, k, v) = (collection.to_string(), key.to_string(), value.to_string());
        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO records (collection, key, value) VALUES (?1,?2,?3)",
                params![c, k, v],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<Value>> {
        let (c, k) = (collection.to_string(), key.to_string());
        self.blocking(move |conn| {
            let v: Option<String> = conn
                .query_row("SELECT value FROM records WHERE collection = ?1 AND key = ?2", params![c, k], |r| r.get(0))
                .optional()?;
            Ok(v.and_then(|s| serde_json::from_str(&s).ok()))
        })
        .await
    }

    async fn health_check(&self) -> bool {
        self.blocking(|conn| {
            conn.query_row("SELECT 1", [], |_| Ok(())).map_err(|e| SlcError::Storage(e.to_string()))?;
            Ok(())
        })
        .await
        .is_ok()
    }

    async fn close(&self) -> SlcResult<()> {
        Ok(())
    }
}

fn row_to_embedding(row: &Row) -> rusqlite::Result<EmbeddingRecord> {
    let emb: String = row.get(3)?;
    let scope: String = row.get(7)?;
    Ok(EmbeddingRecord {
        document_id: row.get(0)?,
        chunk_index: row.get(1)?,
        chunk_total: row.get(2)?,
        embedding: serde_json::from_str(&emb).unwrap_or_default(),
        embedding_model: row.get(4)?,
        embedding_dimension: row.get::<_, i64>(5)? as usize,
        generated_at: DateTime::parse_from_rfc3339(&row.get::<_, String>(6)?)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        scope: if scope == "private" { EmbeddingScope::Private } else { EmbeddingScope::Public },
        seat_id: row.get(8)?,
    })
}

fn row_to_seat(row: &Row) -> rusqlite::Result<Seat> {
    let status: String = row.get(2)?;
    Ok(Seat {
        seat_id: row.get(0)?,
        name: row.get(1)?,
        status: SeatStatus::parse(status.trim_matches('"')),
        created_at: dt(row.get::<_, String>(3)?),
        last_accessed: dt(row.get::<_, String>(4)?),
        expires_at: row.get::<_, Option<String>>(5)?.and_then(|s| DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&Utc)),
        metadata: row.get::<_, String>(6).map(|s| serde_json::from_str(&s).unwrap_or_default())?,
        active_task_id: row.get(7)?,
        context: row.get::<_, String>(8).map(|s| serde_json::from_str(&s).unwrap_or_default())?,
        usage_stats: row.get::<_, String>(9).map(|s| serde_json::from_str(&s).unwrap_or_default())?,
    })
}

fn row_to_timer(row: &Row) -> rusqlite::Result<PersistedTimer> {
    let timer_type: String = row.get(3)?;
    Ok(PersistedTimer {
        timer_id: row.get(0)?,
        seat_id: row.get(1)?,
        timer_type: match timer_type.as_str() {
            "CONSOLIDATION" => TimerType::Consolidation,
            "HISTORY_COMPRESSION" => TimerType::HistoryCompression,
            "REFLECTION" => TimerType::Reflection,
            "FOCUS_REMINDER" => TimerType::FocusReminder,
            "IDEA_REMINDER" => TimerType::IdeaReminder,
            _ => TimerType::Reminder,
        },
        interval_seconds: row.get(4)?,
        last_fired_at: row.get::<_, Option<String>>(5)?.and_then(|s| DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&Utc)),
        next_fire_at: dt(row.get::<_, String>(6)?),
        is_active: row.get::<_, i64>(7)? != 0,
        metadata: row.get::<_, String>(8).map(|s| serde_json::from_str(&s).unwrap_or_default())?,
        created_at: dt(row.get::<_, String>(9)?),
    })
}

fn dt(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DocLevel, DocMeta, Document, DocumentCategory};
    use crate::storage::{DocFilter, DocSort, SortDir};

    #[tokio::test]
    async fn kb_crud_and_filter() {
        let store = SqliteStore::in_memory().unwrap();

        let mut meta = DocMeta::default();
        meta.doc_type = Some("notes".into());
        let doc = Document::new(
            "proj_vassista",
            DocumentCategory::Project,
            "Vassista voice assistant project plan",
            meta,
            vec!["priority:high".into(), "memory:semantic".into()],
            None,
        );
        store.kb_insert(&doc).await.unwrap();

        assert!(store.kb_get("proj_vassista").await.unwrap().is_some());
        assert!(store.kb_get("nope").await.unwrap().is_none());

        // History rejected from the KB store:
        let hist = Document::new(
            "episodic_l1_seat_x",
            DocumentCategory::History,
            "raw event",
            DocMeta::default(),
            vec![],
            Some("seat_x".into()),
        );
        assert!(store.kb_insert(&hist).await.is_err());

        // Visibility: public doc visible to any seat.
        let f = DocFilter::visible_to("seat_a");
        assert_eq!(store.kb_count(&f).await.unwrap(), 1);

        // Content update bumps version.
        assert!(store.kb_update_content("proj_vassista", "v2").await.unwrap());
        let d = store.kb_get("proj_vassista").await.unwrap().unwrap();
        assert_eq!(d.version, 2);
        assert_eq!(d.content_hash, crate::model::content_hash("v2"));

        // Soft delete → graveyard.
        assert!(store.kb_soft_delete("proj_vassista").await.unwrap());
        assert!(store.kb_get("proj_vassista").await.unwrap().is_none());
        assert_eq!(store.kb_graveyard(None).await.unwrap().len(), 1);
        assert!(store.kb_restore("proj_vassista").await.unwrap());
        assert!(store.kb_get("proj_vassista").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn episodic_pipeline_queries() {
        let store = SqliteStore::in_memory().unwrap();
        let seat = "seat_1";

        for i in 0..5 {
            let mut m = DocMeta::default();
            m.doc_level = Some(DocLevel::L1);
            m.seat_id = Some(seat.into());
            let doc = Document::new(
                format!("evt_{i}"),
                DocumentCategory::History,
                format!("event {i}"),
                m,
                vec![],
                Some(seat.into()),
            );
            store.episodic_insert(&doc).await.unwrap();
        }

        // L1 gather: doc_level=L1, seat, no compression batch.
        let f = DocFilter {
            seat_id: Some(seat.into()),
            doc_level: Some(DocLevel::L1),
            has_compression_batch: Some(false),
            ..Default::default()
        };
        let docs = store.episodic_find(&f, &DocSort::by_created(SortDir::Asc), 500).await.unwrap();
        assert_eq!(docs.len(), 5);

        // KB search never sees episodic docs.
        assert_eq!(store.kb_count(&DocFilter::default()).await.unwrap(), 0);

        // Patch: mark all consumed.
        let patch = MetaPatch {
            set_archived: Some(true),
            set_compression_batch_id: Some("batch_x".into()),
            ..Default::default()
        };
        let changed = store.episodic_patch_meta(&f, &patch).await.unwrap();
        assert_eq!(changed, 5);
        assert_eq!(
            store
                .episodic_count(&DocFilter { has_compression_batch: Some(false), ..Default::default() })
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn embeddings_roundtrip() {
        let store = SqliteStore::in_memory().unwrap();
        let rec = EmbeddingRecord {
            document_id: "doc_a".into(),
            chunk_index: 0,
            chunk_total: 1,
            embedding: vec![0.1, 0.2, 0.3],
            embedding_model: "bge-m3".into(),
            embedding_dimension: 3,
            generated_at: Utc::now(),
            scope: EmbeddingScope::Public,
            seat_id: None,
        };
        store.insert_embeddings(&[rec]).await.unwrap();
        let v = store.get_embedding("doc_a").await.unwrap().unwrap();
        assert_eq!(v, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn seats_and_stats() {
        let store = SqliteStore::in_memory().unwrap();
        let seat = Seat {
            seat_id: "seat_x".into(),
            name: "test".into(),
            status: SeatStatus::Active,
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            expires_at: None,
            metadata: Default::default(),
            active_task_id: None,
            context: Default::default(),
            usage_stats: Default::default(),
        };
        store.insert_seat(&seat).await.unwrap();
        assert_eq!(store.list_active_seats(10).await.unwrap().len(), 1);
        assert!(store.incr_seat_stats("seat_x", "search", 12).await.unwrap());
        let s = store.get_seat("seat_x").await.unwrap().unwrap();
        assert_eq!(s.usage_stats.total_requests, 1);
        assert_eq!(s.usage_stats.total_tokens, 12);
    }
}
