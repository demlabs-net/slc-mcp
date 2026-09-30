//! Embedded SQLite backend for [`StorageBackend`].
//!
//! Two physical tables (`documents` for the KB, `episodic` for HISTORY) so
//! the "history is not RAG'd" invariant holds at the schema level. All ops
//! run on a `spawn_blocking` worker (rusqlite is sync; the trait is async).

use super::{
    DocFilter, DocSort, KbMetadataProjection, MetaPatch, SortDir, SortField, StorageBackend,
};
use crate::error::{SlcError, SlcResult};
use crate::model::{
    Document, DocumentCategory, EmbeddingRecord, EmbeddingScope, PersistedTimer, Seat, SeatStatus,
    TimerType, UsageStats, content_hash,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
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
    usage_stats    TEXT NOT NULL,
    active_document_id TEXT
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

CREATE TABLE IF NOT EXISTS task_attempts (
    attempt_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    transport_key TEXT NOT NULL,
    attempt_no INTEGER NOT NULL,
    reservation TEXT NOT NULL,
    UNIQUE(task_id, transport_key),
    UNIQUE(task_id, attempt_no)
);
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
        // Migration: older vaults lack the unified active-document column.
        let has_col: bool = conn
            .prepare("PRAGMA table_info(seats)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|name| name == "active_document_id");
        if !has_col {
            conn.execute("ALTER TABLE seats ADD COLUMN active_document_id TEXT", [])?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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

fn doc_to_row(
    doc: &Document,
) -> SlcResult<(
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    i64,
    Option<String>,
)> {
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
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                format!("bad category {category}").into(),
            )
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

    if let Some(ids) = &f.document_ids {
        if ids.is_empty() {
            clauses.push("0 = 1".to_string()); // empty allowlist matches nothing
        } else {
            let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
            clauses.push(format!("document_id IN ({})", placeholders.join(", ")));
            params.extend(ids.iter().cloned());
        }
    }
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
    for (key, value) in &f.extra_strings {
        clauses.push(
            "json_type(metadata, ?) = 'text' AND CAST(json_extract(metadata, ?) AS TEXT) = ?"
                .to_string(),
        );
        let path = format!(
            "$.{}",
            serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string())
        );
        params.push(path.clone());
        params.push(path);
        params.push(value.clone());
    }
    for (key, values) in &f.extra_strings_not_in {
        if values.is_empty() {
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        clauses.push(format!(
            "(json_type(metadata, ?) IS NULL OR json_type(metadata, ?) != 'text' OR CAST(json_extract(metadata, ?) AS TEXT) NOT IN ({placeholders}))"
        ));
        let path = format!(
            "$.{}",
            serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string())
        );
        params.push(path.clone());
        params.push(path.clone());
        params.push(path);
        params.extend(values.iter().cloned());
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
        Some(true) => {
            clauses.push("json_extract(metadata, '$.compression_batch_id') IS NOT NULL".to_string())
        }
        Some(false) => {
            clauses.push("json_extract(metadata, '$.compression_batch_id') IS NULL".to_string())
        }
        None => {}
    }
    if !f.tags_any.is_empty() {
        // tags stored as JSON array — match if ANY listed tag is a member.
        let first_param = params.len() + 1;
        let ors: Vec<String> = f
            .tags_any
            .iter()
            .enumerate()
            .map(|(offset, _)| {
                format!(
                    "EXISTS (SELECT 1 FROM json_each(tags) WHERE json_each.value = ?{})",
                    first_param + offset
                )
            })
            .collect();
        clauses.push(format!("({})", ors.join(" OR ")));
        params.extend(f.tags_any.iter().cloned());
    }
    if !f.tags_all.is_empty() {
        let first_param = params.len() + 1;
        let ands: Vec<String> = f
            .tags_all
            .iter()
            .enumerate()
            .map(|(offset, _)| {
                format!(
                    "EXISTS (SELECT 1 FROM json_each(tags) WHERE json_each.value = ?{})",
                    first_param + offset
                )
            })
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

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
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
    params
        .iter()
        .map(|p| p as &dyn rusqlite::types::ToSql)
        .collect()
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
                "INSERT OR REPLACE INTO documents (document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at)
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
        let (id, content, hash) = (
            document_id.to_string(),
            content.to_string(),
            content_hash(content),
        );
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
        if self
            .kb_update_content(&doc.document_id, &doc.content)
            .await?
        {
            return Ok(true);
        }
        self.kb_insert(doc).await?;
        Ok(false)
    }

    async fn kb_replace(&self, doc: &Document) -> SlcResult<bool> {
        if !doc.category.is_kb() {
            return Err(SlcError::Storage(format!(
                "history docs go to the episodic store, not the KB: {}",
                doc.document_id
            )));
        }
        let row = doc_to_row(doc)?;
        let existed = self.kb_get(&doc.document_id).await?.is_some();
        self.blocking(move |conn| {
            conn.execute(
                "INSERT INTO documents (document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                 ON CONFLICT(document_id) DO UPDATE SET
                    category=excluded.category, folder=excluded.folder, content=excluded.content,
                    content_hash=excluded.content_hash, metadata=excluded.metadata, tags=excluded.tags,
                    auto_load=excluded.auto_load, refs=excluded.refs, seat_id=excluded.seat_id,
                    updated_at=excluded.updated_at, version=excluded.version, deleted_at=excluded.deleted_at",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11, row.12, row.13],
            )?;
            Ok(())
        })
        .await?;
        Ok(existed)
    }

    async fn kb_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>> {
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

    async fn kb_find_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<Document>> {
        let f = filter.clone();
        let sort = *sort;
        self.blocking(move |conn| {
            let (where_sql, params) = build_where(&f);
            let sql = format!(
                "SELECT document_id, category, folder, content, content_hash, metadata, tags, auto_load, refs, seat_id, created_at, updated_at, version, deleted_at
                 FROM documents {where_sql} ORDER BY {} {}",
                order_by(&sort),
                order_dir(&sort),
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(kv_params(&params).as_slice(), row_to_doc)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    async fn kb_find_metadata_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<KbMetadataProjection>> {
        let f = filter.clone();
        let sort = *sort;
        self.blocking(move |conn| {
            let (where_sql, params) = build_where(&f);
            let sql = format!(
                "SELECT document_id, category, seat_id, metadata
                 FROM documents {where_sql} ORDER BY {} {}",
                order_by(&sort),
                order_dir(&sort),
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(kv_params(&params).as_slice(), |row| {
                let category: String = row.get(1)?;
                let metadata: String = row.get(3)?;
                let metadata =
                    serde_json::from_str::<crate::model::DocMeta>(&metadata).unwrap_or_default();
                Ok((
                    row.get::<_, String>(0)?,
                    category,
                    row.get(2)?,
                    metadata.extra,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (document_id, category, seat_id, metadata_extra) = row?;
                let category = DocumentCategory::parse(&category).ok_or_else(|| {
                    SlcError::Storage(format!(
                        "invalid category {category} in metadata projection for {document_id}"
                    ))
                })?;
                out.push(KbMetadataProjection {
                    document_id,
                    category,
                    seat_id,
                    metadata_extra,
                });
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

    async fn kb_rename(&self, old_id: &str, new_id: &str) -> SlcResult<bool> {
        let old = old_id.to_string();
        let new = new_id.to_string();
        self.blocking(move |conn| {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM documents WHERE document_id = ?1 AND deleted_at IS NULL",
                    params![new],
                    |_| Ok(true),
                )
                .optional()?
                .is_some();
            if exists {
                return Err(SlcError::Storage(format!("document already exists: {new}")));
            }
            let n = conn.execute(
                "UPDATE documents SET document_id = ?1 WHERE document_id = ?2",
                params![new, old],
            )?;
            if n == 0 {
                return Ok(false);
            }
            conn.execute(
                "UPDATE embeddings SET document_id = ?1 WHERE document_id = ?2",
                params![new, old],
            )?;
            Ok(true)
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
            let n = conn.execute(
                "DELETE FROM documents WHERE deleted_at IS NOT NULL AND deleted_at <= ?1",
                params![cutoff],
            )?;
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
        // Seat-less episodic docs are allowed (legacy import): they are not
        // picked up by any per-seat pipeline, just stored as diary.
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
            .episodic_count(&DocFilter {
                seat_id: doc.seat_id.clone(),
                ..Default::default()
            })
            .await;
        let _ = existing;
        self.blocking(move |conn| {
            let n = conn.execute(
                "UPDATE episodic SET folder = ?2, content = ?3, content_hash = ?4, metadata = ?5, tags = ?6, version = version + 1, updated_at = ?7
                 WHERE document_id = ?1",
                params![row.0, row.2, row.3, row.4, row.5, row.6, row.11],
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

    async fn episodic_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>> {
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
        let docs = self
            .episodic_find(&f, &DocSort::default(), usize::MAX)
            .await?;
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
        let rows: Vec<(
            String,
            i64,
            i64,
            String,
            String,
            i64,
            String,
            String,
            Option<String>,
        )> = records
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

    async fn all_embeddings(
        &self,
        scope: EmbeddingScope,
        seat_id: Option<&str>,
    ) -> SlcResult<Vec<EmbeddingRecord>> {
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
        let (
            seat_id,
            name,
            status,
            created_at,
            last_accessed,
            expires_at,
            metadata,
            active_task_id,
            context,
            usage_stats,
            active_document_id,
        ) = (
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
            seat.active_document_id.clone(),
        );
        self.blocking(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO seats (seat_id, name, status, created_at, last_accessed, expires_at, metadata, active_task_id, context, usage_stats, active_document_id)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![seat_id, name, status, created_at, last_accessed, expires_at, metadata, active_task_id, context, usage_stats, active_document_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
        let id = seat_id.to_string();
        self.blocking(move |conn| {
            let row = conn
                .query_row(
                    "SELECT * FROM seats WHERE seat_id = ?1",
                    params![id],
                    row_to_seat,
                )
                .optional()?;
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
            let n = conn.execute(
                "UPDATE seats SET status = ?2 WHERE seat_id = ?1",
                params![id, status],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let (id, task) = (seat_id.to_string(), task_id.to_string());
        self.blocking(move |conn| {
            // Task activation is document activation too (unified anchor).
            let n = conn.execute(
                "UPDATE seats SET active_task_id = ?2, active_document_id = ?2 WHERE seat_id = ?1",
                params![id, task],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn set_seat_active_document(
        &self,
        seat_id: &str,
        document_id: Option<&str>,
    ) -> SlcResult<bool> {
        let (id, doc) = (seat_id.to_string(), document_id.map(String::from));
        self.blocking(move |conn| {
            let n = conn.execute(
                "UPDATE seats SET active_document_id = ?2 WHERE seat_id = ?1",
                params![id, doc],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
        let id = seat_id.to_string();
        self.blocking(move |conn| {
            let row = conn
                .query_row(
                    "SELECT active_document_id, active_task_id FROM seats WHERE seat_id = ?1",
                    params![id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()?;
            // Unified field first; fall back to the legacy task pointer so
            // seats activated before the migration keep working.
            Ok(row.and_then(|(doc, task)| doc.or(task)))
        })
        .await
    }

    async fn incr_seat_stats(
        &self,
        seat_id: &str,
        tool_name: &str,
        tokens_used: i64,
    ) -> SlcResult<bool> {
        let id = seat_id.to_string();
        let tool = tool_name.to_string();
        self.blocking(move |conn| {
            let stats: Option<String> = conn
                .query_row(
                    "SELECT usage_stats FROM seats WHERE seat_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            let mut stats: UsageStats = stats
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            stats.total_requests += 1;
            stats.total_tokens += tokens_used;
            let count = stats
                .tools_used
                .get(&tool)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                + 1;
            stats
                .tools_used
                .insert(tool, serde_json::Value::from(count));
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
        let (
            timer_id,
            seat_id,
            timer_type,
            interval_seconds,
            last_fired_at,
            next_fire_at,
            is_active,
            metadata,
            created_at,
        ) = (
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

    async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>> {
        let id = timer_id.to_string();
        self.blocking(move |conn| {
            let row = conn
                .query_row(
                    "SELECT * FROM timers WHERE timer_id = ?1",
                    params![id],
                    row_to_timer,
                )
                .optional()?;
            Ok(row)
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

    // ── immutable workflow admission reservations ─────────────────

    async fn reserve_task_attempt(&self, task_id: &str, transport_key: &str, reservation: &Value) -> SlcResult<Value> {
        let (task_id, transport_key, reservation) = (task_id.to_owned(), transport_key.to_owned(), reservation.clone());
        self.blocking(move |conn| {
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            let previous: Option<String> = tx.query_row(
                "SELECT reservation FROM task_attempts WHERE task_id=?1 AND transport_key=?2",
                params![task_id, transport_key], |row| row.get(0),
            ).optional()?;
            if let Some(previous) = previous {
                let previous: Value = serde_json::from_str(&previous)?;
                let mut intended = reservation.clone();
                if let (Some(old), Some(new)) = (previous.as_object(), intended.as_object_mut()) {
                    new.insert("attempt_id".into(), old["attempt_id"].clone());
                    new.insert("attempt_no".into(), old["attempt_no"].clone());
                    new.insert("issued_at".into(), old["issued_at"].clone());
                    new.insert("expires_at".into(), old["expires_at"].clone());
                }
                // Server-owned terminal/binding state is not part of the
                // transport replay fingerprint; replay never resurrects it.
                if let Some(old) = previous.as_object() {
                    for field in ["bound_run_id", "start_event_id", "abort_receipt", "aborted_at"] {
                        if let Some(value) = old.get(field) { intended[field] = value.clone(); }
                    }
                }
                if previous != intended {
                    return Err(SlcError::InvalidInput("transport_key replay has a different reservation fingerprint".into()));
                }
                tx.commit()?;
                return Ok(previous);
            }
            let attempt_no: i64 = tx.query_row(
                "SELECT COALESCE(MAX(attempt_no), 0) + 1 FROM task_attempts WHERE task_id=?1",
                params![task_id], |row| row.get(0),
            )?;
            let mut created = reservation;
            created["attempt_no"] = Value::from(attempt_no);
            tx.execute("INSERT INTO task_attempts(attempt_id,task_id,transport_key,attempt_no,reservation) VALUES(?1,?2,?3,?4,?5)",
                params![created["attempt_id"].as_str().ok_or_else(|| SlcError::InvalidInput("attempt_id missing".into()))?, task_id, transport_key, attempt_no, created.to_string()])?;
            tx.commit()?;
            Ok(created)
        }).await
    }

    async fn list_task_attempts(&self, task_id: &str) -> SlcResult<Vec<Value>> {
        let id = task_id.to_owned();
        self.blocking(move |conn| {
            let mut stmt = conn.prepare("SELECT reservation FROM task_attempts WHERE task_id=?1 ORDER BY attempt_no")?;
            let rows = stmt.query_map(params![id], |row| row.get::<_, String>(0))?;
            rows.map(|row| serde_json::from_str(&row?).map_err(Into::into)).collect()
        }).await
    }

    async fn bind_task_attempt(&self, attempt_id: &str, run_id: &str) -> SlcResult<Value> {
        let (attempt_id, run_id) = (attempt_id.to_owned(), run_id.to_owned());
        if run_id.trim().is_empty() || run_id.len() > 500 {
            return Err(SlcError::InvalidInput("invalid Hermes run_id".into()));
        }
        self.blocking(move |conn| {
            // IMMEDIATE takes the SQLite writer lock across processes before reading
            // either the reservation, task projection, or the event stream.
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            let raw: String = tx.query_row("SELECT reservation FROM task_attempts WHERE attempt_id=?1",
                params![attempt_id], |row| row.get(0))?;
            let mut reservation: Value = serde_json::from_str(&raw)?;
            if reservation.get("abort_receipt").is_some() {
                return Err(SlcError::PermissionDenied("aborted attempt cannot be bound".into()));
            }
            let task_id = reservation["task_id"].as_str().ok_or_else(|| SlcError::Storage("corrupt attempt task_id".into()))?.to_owned();
            let event_id = format!("attempt_started_{attempt_id}");
            if let Some(previous) = reservation["bound_run_id"].as_str() {
                if previous != run_id { return Err(SlcError::InvalidInput("attempt is bound to another run".into())); }
                let present: Option<String> = tx.query_row("SELECT value FROM records WHERE collection='task_events_v1' AND key=?1",
                    params![event_id], |row| row.get(0)).optional()?;
                if present.is_none() { return Err(SlcError::Storage("bound attempt missing atomic Started event".into())); }
                tx.commit()?;
                return Ok(reservation);
            }
            let mut doc: Document = tx.query_row("SELECT * FROM documents WHERE document_id=?1", params![task_id], row_to_doc)?;
            if doc.category != DocumentCategory::Task
                || doc.metadata.extra.get("status").and_then(Value::as_str) != Some("PENDING")
                || doc.metadata.extra.get("queue_state").and_then(Value::as_str) != Some("ready")
                || doc.metadata.extra.get("queue_ready_event_id") != reservation.get("queue_ready_event_id")
                || doc.metadata.extra.get("assignee") != reservation.get("assignee") {
                return Err(SlcError::PermissionDenied("attempt task is no longer the READY head".into()));
            }
            let competing: Option<String> = tx.query_row(
                "SELECT attempt_id FROM task_attempts WHERE task_id=?1 AND attempt_id<>?2 AND json_extract(reservation,'$.bound_run_id') IS NOT NULL LIMIT 1",
                params![task_id, attempt_id], |row| row.get(0)).optional()?;
            if competing.is_some() { return Err(SlcError::PermissionDenied("another attempt already started this task".into())); }
            let now = Utc::now();
            let actor = reservation["assignee"].as_str().ok_or_else(|| SlcError::Storage("corrupt attempt assignee".into()))?.to_owned();
            let event = serde_json::json!({"event_id":event_id,"task_id":task_id,"kind":"started",
                "actor":actor,"recipient":doc.metadata.extra.get("issuer").and_then(Value::as_str),
                "status":"IN_WORK","message":"Hermes held attempt bound",
                "metadata":{"attempt_id":attempt_id,"run_id":run_id},"created_at":now.to_rfc3339()});
            let existing: Option<String> = tx.query_row("SELECT key FROM records WHERE collection='task_events_v1' AND json_extract(value,'$.task_id')=?1 AND json_extract(value,'$.kind')='started' LIMIT 1",
                params![task_id], |row| row.get(0)).optional()?;
            if existing.is_some() { return Err(SlcError::PermissionDenied("task already has a Started event".into())); }
            doc.metadata.extra.insert("status".into(), Value::from("IN_WORK"));
            doc.metadata.extra.insert("queue_state".into(), Value::from("running"));
            doc.metadata.extra.remove("queue_ready_event_id");
            doc.metadata.extra.insert("last_event_id".into(), Value::from(event_id.clone()));
            doc.metadata.extra.insert("last_event_at".into(), Value::from(now.to_rfc3339()));
            doc.updated_at = now;
            doc.version += 1;
            reservation["bound_run_id"] = Value::from(run_id);
            reservation["start_event_id"] = Value::from(event_id.clone());
            tx.execute("INSERT INTO records(collection,key,value) VALUES('task_events_v1',?1,?2)", params![event_id,event.to_string()])?;
            tx.execute("UPDATE documents SET metadata=?2,updated_at=?3,version=?4 WHERE document_id=?1",
                params![task_id,serde_json::to_string(&doc.metadata)?,now.to_rfc3339(),doc.version])?;
            tx.execute("UPDATE task_attempts SET reservation=?2 WHERE attempt_id=?1", params![attempt_id,reservation.to_string()])?;
            tx.commit()?;
            Ok(reservation)
        }).await
    }

    async fn abort_task_attempt(&self, attempt_id: &str, expected: &Value, receipt: &Value) -> SlcResult<Value> {
        let (attempt_id, expected, receipt) = (attempt_id.to_owned(), expected.clone(), receipt.clone());
        self.blocking(move |conn| {
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            let raw: String = tx.query_row("SELECT reservation FROM task_attempts WHERE attempt_id=?1",
                params![attempt_id], |row| row.get(0))?;
            let mut reservation: Value = serde_json::from_str(&raw)?;
            // The caller must have verified the exact Hermes receipt before
            // entering this transaction. Compare under the writer lock so a
            // racing bind or second abort cannot silently overwrite evidence.
            if reservation != expected || reservation.get("bound_run_id").is_some()
                || reservation.get("start_event_id").is_some() {
                return Err(SlcError::PermissionDenied("attempt changed or was bound during abort".into()));
            }
            let fields = ["attempt_id", "task_id", "assignee", "profile", "queue_ready_event_id",
                "payload_sha256", "transport_key", "attempt_no"];
            if fields.iter().any(|field| reservation.get(*field).is_none() || reservation.get(*field) != receipt.get(*field))
                || receipt.get("run_id") != Some(&Value::Null)
                || receipt["status"].as_str() != Some("aborted") || receipt["active"].as_bool() != Some(false)
                || receipt["aborted"].as_bool() != Some(true)
                || !receipt["abort_receipt"].as_str().is_some_and(|s| !s.trim().is_empty() && s.len() <= 500) {
                return Err(SlcError::PermissionDenied("invalid durable Hermes abort receipt".into()));
            }
            // Persist only the immutable proof and identity. Hermes' POST and
            // subsequent GET can differ in unrelated diagnostic/volatile fields.
            let mut durable = serde_json::Map::new();
            for field in fields {
                durable.insert(field.into(), receipt[field].clone());
            }
            for field in ["run_id", "status", "active", "aborted", "abort_receipt"] {
                durable.insert(field.into(), receipt[field].clone());
            }
            let durable = Value::Object(durable);
            if let Some(previous) = reservation.get("abort_receipt") {
                if durable.as_object().unwrap().iter().any(|(key, value)| previous.get(key) != Some(value)) {
                    return Err(SlcError::PermissionDenied("abort receipt mismatch".into()));
                }
            } else {
                reservation["abort_receipt"] = durable;
                reservation["aborted_at"] = Value::from(Utc::now().to_rfc3339());
                tx.execute("UPDATE task_attempts SET reservation=?2 WHERE attempt_id=?1",
                    params![attempt_id, reservation.to_string()])?;
            }
            tx.commit()?;
            Ok(reservation)
        }).await
    }

    async fn has_task_attempt(&self, task_id: &str) -> SlcResult<bool> {
        let id = task_id.to_owned();
        self.blocking(move |conn| {
            Ok(conn.query_row("SELECT 1 FROM task_attempts WHERE task_id=?1 AND json_extract(reservation,'$.abort_receipt') IS NULL LIMIT 1", params![id], |_| Ok(())).optional()?.is_some())
        }).await
    }

    async fn get_task_attempt(&self, attempt_id: &str) -> SlcResult<Option<Value>> {
        let id = attempt_id.to_owned();
        self.blocking(move |conn| {
            let value: Option<String> = conn.query_row("SELECT reservation FROM task_attempts WHERE attempt_id=?1", params![id], |row| row.get(0)).optional()?;
            value.map(|value| serde_json::from_str(&value).map_err(Into::into)).transpose()
        }).await
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
                .query_row(
                    "SELECT value FROM records WHERE collection = ?1 AND key = ?2",
                    params![c, k],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(v.and_then(|s| serde_json::from_str(&s).ok()))
        })
        .await
    }

    async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, Value)>> {
        let c = collection.to_string();
        self.blocking(move |conn| {
            let mut stmt =
                conn.prepare("SELECT key, value FROM records WHERE collection = ?1 ORDER BY key")?;
            let rows = stmt.query_map(params![c], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (k, v) = row?;
                if let Ok(value) = serde_json::from_str(&v) {
                    out.push((k, value));
                }
            }
            Ok(out)
        })
        .await
    }

    async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool> {
        let (c, k) = (collection.to_string(), key.to_string());
        self.blocking(move |conn| {
            let n = conn.execute(
                "DELETE FROM records WHERE collection = ?1 AND key = ?2",
                params![c, k],
            )?;
            Ok(n > 0)
        })
        .await
    }

    async fn health_check(&self) -> bool {
        self.blocking(|conn| {
            conn.query_row("SELECT 1", [], |_| Ok(()))
                .map_err(|e| SlcError::Storage(e.to_string()))?;
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
        scope: if scope == "private" {
            EmbeddingScope::Private
        } else {
            EmbeddingScope::Public
        },
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
        expires_at: row
            .get::<_, Option<String>>(5)?
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)),
        metadata: row
            .get::<_, String>(6)
            .map(|s| serde_json::from_str(&s).unwrap_or_default())?,
        active_task_id: row.get(7)?,
        context: row
            .get::<_, String>(8)
            .map(|s| serde_json::from_str(&s).unwrap_or_default())?,
        usage_stats: row
            .get::<_, String>(9)
            .map(|s| serde_json::from_str(&s).unwrap_or_default())?,
        active_document_id: row.get(10)?,
    })
}

fn row_to_timer(row: &Row) -> rusqlite::Result<PersistedTimer> {
    let timer_type: String = row.get(2)?;
    Ok(PersistedTimer {
        timer_id: row.get(0)?,
        seat_id: row.get(1)?,
        timer_type: match timer_type.as_str() {
            "CONSOLIDATION" => TimerType::Consolidation,
            "HISTORY_COMPRESSION" => TimerType::HistoryCompression,
            "FOCUS_REMINDER" => TimerType::FocusReminder,
            _ => TimerType::Reminder,
        },
        interval_seconds: row.get(3)?,
        last_fired_at: row
            .get::<_, Option<String>>(4)?
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)),
        next_fire_at: dt(row.get::<_, String>(5)?),
        is_active: row.get::<_, i64>(6)? != 0,
        metadata: row
            .get::<_, String>(7)
            .map(|s| serde_json::from_str(&s).unwrap_or_default())?,
        created_at: dt(row.get::<_, String>(8)?),
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
    async fn attempt_reservation_is_atomic_immutable_and_replay_checked() {
        let store = SqliteStore::in_memory().unwrap();
        let proposed = serde_json::json!({"attempt_id":"a1","task_id":"task-a","transport_key":"wake-1","profile":"worker","queue_ready_event_id":"ready-1","assignee":"worker","payload_sha256":"f".repeat(64),"issued_at":1,"expires_at":301,"attempt_no":0});
        let first = store.reserve_task_attempt("task-a", "wake-1", &proposed).await.unwrap();
        assert_eq!(first["attempt_no"], 1);
        assert_eq!(store.reserve_task_attempt("task-a", "wake-1", &proposed).await.unwrap(), first);
        assert!(store.reserve_task_attempt("task-a", "wake-1", &serde_json::json!({"attempt_id":"a2","task_id":"task-a","transport_key":"wake-1","profile":"other","queue_ready_event_id":"ready-1","assignee":"worker","payload_sha256":"f".repeat(64),"issued_at":2,"expires_at":302,"attempt_no":0})).await.is_err());
        let mut next = proposed.clone();
        next["attempt_id"] = Value::from("a2");
        assert_eq!(store.reserve_task_attempt("task-a", "wake-2", &next).await.unwrap()["attempt_no"], 2);
        assert!(store.has_task_attempt("task-a").await.unwrap());
        assert_eq!(store.get_task_attempt("a1").await.unwrap().unwrap(), first);
    }

    fn aborted_receipt(reservation: &Value, proof: &str) -> Value {
        let mut receipt = reservation.clone();
        receipt["status"] = Value::from("aborted");
        receipt["active"] = Value::from(false);
        receipt["aborted"] = Value::from(true);
        receipt["run_id"] = Value::Null;
        receipt["abort_receipt"] = Value::from(proof);
        receipt
    }

    #[tokio::test]
    async fn abort_requires_all_attempts_and_rejects_racing_bind_or_changed_receipt() {
        let path = std::env::temp_dir().join(format!("slc-attempt-abort-{}.sqlite", uuid::Uuid::new_v4()));
        let store = SqliteStore::open(&path).unwrap();
        let second = SqliteStore::open(&path).unwrap();
        let mut meta = DocMeta::default();
        for (key, value) in [("status", "PENDING"), ("queue_state", "ready"),
            ("queue_ready_event_id", "ready-1"), ("assignee", "worker")] {
            meta.extra.insert(key.into(), Value::from(value));
        }
        store.kb_insert(&Document::new("task-abort", DocumentCategory::Task, "abort", meta, vec![], None)).await.unwrap();
        let proposed = serde_json::json!({"attempt_id":"a1","task_id":"task-abort","transport_key":"wake-1",
            "profile":"worker","queue_ready_event_id":"ready-1","assignee":"worker",
            "payload_sha256":"f".repeat(64),"issued_at":1,"expires_at":2,"attempt_no":0});
        let first = store.reserve_task_attempt("task-abort", "wake-1", &proposed).await.unwrap();
        let mut next = proposed.clone();
        next["attempt_id"] = Value::from("a2");
        let second_attempt = store.reserve_task_attempt("task-abort", "wake-2", &next).await.unwrap();
        assert!(store.has_task_attempt("task-abort").await.unwrap()); // expiry is not proof
        let receipt = aborted_receipt(&first, "durable-1");
        let aborted = second.abort_task_attempt("a1", &first, &receipt).await.unwrap();
        assert_eq!(store.reserve_task_attempt("task-abort", "wake-1", &proposed).await.unwrap(), aborted);
        assert!(store.has_task_attempt("task-abort").await.unwrap()); // another unknown attempt
        assert!(store.bind_task_attempt("a1", "run-1").await.is_err());
        assert!(second.abort_task_attempt("a1", &first, &receipt).await.is_err()); // stale snapshot
        let mut replay_receipt = receipt.clone();
        replay_receipt["diagnostic"] = Value::from("later registry read");
        assert_eq!(second.abort_task_attempt("a1", &aborted, &replay_receipt).await.unwrap(), aborted);
        assert!(second.abort_task_attempt("a1", &aborted, &aborted_receipt(&first, "different")).await.is_err());
        store.bind_task_attempt("a2", "run-2").await.unwrap();
        assert!(second.abort_task_attempt("a2", &second_attempt, &receipt).await.is_err());
        assert!(store.has_task_attempt("task-abort").await.unwrap());
        drop(second);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn all_aborted_attempts_release_ordinary_workflow_hold() {
        let store = SqliteStore::in_memory().unwrap();
        let proposed = serde_json::json!({"attempt_id":"a1","task_id":"task-abort-all","transport_key":"wake-1",
            "profile":"worker","queue_ready_event_id":"ready-1","assignee":"worker",
            "payload_sha256":"f".repeat(64),"issued_at":1,"expires_at":2,"attempt_no":0});
        let first = store.reserve_task_attempt("task-abort-all", "wake-1", &proposed).await.unwrap();
        let mut next = proposed.clone(); next["attempt_id"] = Value::from("a2");
        let second = store.reserve_task_attempt("task-abort-all", "wake-2", &next).await.unwrap();
        store.abort_task_attempt("a1", &first, &aborted_receipt(&first, "proof-1")).await.unwrap();
        assert!(store.has_task_attempt("task-abort-all").await.unwrap());
        store.abort_task_attempt("a2", &second, &aborted_receipt(&second, "proof-2")).await.unwrap();
        assert!(!store.has_task_attempt("task-abort-all").await.unwrap());
    }

    #[tokio::test]
    async fn binding_is_atomic_replayable_and_cross_connection_safe() {
        let path = std::env::temp_dir().join(format!("slc-attempt-bind-{}.sqlite", uuid::Uuid::new_v4()));
        let store = SqliteStore::open(&path).unwrap();
        let second = SqliteStore::open(&path).unwrap();
        let mut meta = DocMeta::default();
        for (key, value) in [("status", "PENDING"), ("queue_state", "ready"),
            ("queue_ready_event_id", "ready-1"), ("assignee", "worker")] {
            meta.extra.insert(key.into(), Value::from(value));
        }
        let doc = Document::new("task-bind", DocumentCategory::Task, "bind", meta, vec![], None);
        store.kb_insert(&doc).await.unwrap();
        let proposed = serde_json::json!({"attempt_id":"attempt-1","task_id":"task-bind","transport_key":"key-1",
            "profile":"worker","queue_ready_event_id":"ready-1","assignee":"worker",
            "payload_sha256":"f".repeat(64),"issued_at":1,"expires_at":301,"attempt_no":0});
        store.reserve_task_attempt("task-bind", "key-1", &proposed).await.unwrap();
        let bound = second.bind_task_attempt("attempt-1", "run-1").await.unwrap();
        assert_eq!(bound["start_event_id"], "attempt_started_attempt-1");
        assert_eq!(store.bind_task_attempt("attempt-1", "run-1").await.unwrap(), bound);
        assert!(store.bind_task_attempt("attempt-1", "run-2").await.is_err());
        assert_eq!(second.reserve_task_attempt("task-bind", "key-1", &proposed).await.unwrap(), bound);
        let task = store.kb_get("task-bind").await.unwrap().unwrap();
        assert_eq!(task.metadata.extra["queue_state"], "running");
        assert_eq!(task.metadata.extra["status"], "IN_WORK");
        let events = second.list_records("task_events_v1").await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].1["metadata"]["run_id"], "run-1");
        let mut another = proposed.clone();
        another["attempt_id"] = Value::from("attempt-2");
        store.reserve_task_attempt("task-bind", "key-2", &another).await.unwrap();
        assert!(second.bind_task_attempt("attempt-2", "run-3").await.is_err());
        assert_eq!(second.list_task_attempts("task-bind").await.unwrap().len(), 2);
        drop(second);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn kb_crud_and_filter() {
        let store = SqliteStore::in_memory().unwrap();

        let mut meta = DocMeta::default();
        meta.doc_type = Some("notes".into());
        meta.extra
            .insert("assignee".into(), Value::String("junior".into()));
        meta.extra
            .insert("status".into(), Value::String("PENDING".into()));
        meta.extra.insert("attempts".into(), Value::from(1));
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
        let workflow_filter = DocFilter {
            extra_strings: std::collections::BTreeMap::from([(
                "assignee".to_string(),
                "junior".to_string(),
            )]),
            extra_strings_not_in: std::collections::BTreeMap::from([(
                "status".to_string(),
                vec!["COMPLETED".to_string(), "FAILED".to_string()],
            )]),
            ..Default::default()
        };
        assert_eq!(store.kb_count(&workflow_filter).await.unwrap(), 1);
        let terminal_filter = DocFilter {
            extra_strings_not_in: std::collections::BTreeMap::from([(
                "status".to_string(),
                vec!["PENDING".to_string()],
            )]),
            ..Default::default()
        };
        assert_eq!(store.kb_count(&terminal_filter).await.unwrap(), 0);
        let numeric_is_not_a_string = DocFilter {
            extra_strings: std::collections::BTreeMap::from([(
                "attempts".to_string(),
                "1".to_string(),
            )]),
            ..Default::default()
        };
        assert_eq!(store.kb_count(&numeric_is_not_a_string).await.unwrap(), 0);
        let numeric_is_retained_by_string_exclusion = DocFilter {
            extra_strings_not_in: std::collections::BTreeMap::from([(
                "attempts".to_string(),
                vec!["1".to_string()],
            )]),
            ..Default::default()
        };
        assert_eq!(
            store
                .kb_count(&numeric_is_retained_by_string_exclusion)
                .await
                .unwrap(),
            1
        );
        let any_tag_filter = DocFilter {
            tags_any: vec!["missing".to_string(), "memory:semantic".to_string()],
            ..Default::default()
        };
        assert_eq!(store.kb_count(&any_tag_filter).await.unwrap(), 1);
        let all_tag_filter = DocFilter {
            tags_all: vec!["priority:high".to_string(), "memory:semantic".to_string()],
            ..Default::default()
        };
        assert_eq!(store.kb_count(&all_tag_filter).await.unwrap(), 1);

        // Content update bumps version.
        assert!(
            store
                .kb_update_content("proj_vassista", "v2")
                .await
                .unwrap()
        );
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
        let docs = store
            .episodic_find(&f, &DocSort::by_created(SortDir::Asc), 500)
            .await
            .unwrap();
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
                .episodic_count(&DocFilter {
                    has_compression_batch: Some(false),
                    ..Default::default()
                })
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
            active_document_id: None,
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
