//! MongoDB storage backend — the shared-server option (self-hosted or
//! Atlas). One database, five collections:
//!
//! - `docs` — KB + episodic documents in a single collection, distinguished
//!   by `kind` (`"kb"` / `"episodic"`) — the `kb_*` / `episodic_*` families
//!   only ever touch their own kind.
//! - `embeddings`, `seats`, `timers`, `records` — as their names say.
//!
//! Documents are stored as BSON via serde (`_id` = `document_id`), so every
//! field of the unified `Document` model survives a roundtrip. Filtering
//! (`DocFilter`) maps onto MongoDB query operators ($in/$all/$exists/$or,
//! range on created_at); sorting maps onto $sort; `MetaPatch` onto $set on
//! `metadata.*` + `updated_at` bump.
//!
//! Enable with `SLC_STORAGE=mongodb` + `SLC_MONGODB_URI` (default
//! `mongodb://localhost:27017/slc`).

use crate::error::{SlcError, SlcResult};
use crate::model::{Document, EmbeddingRecord, EmbeddingScope, PersistedTimer, Seat, SeatStatus};
use crate::storage::{DocFilter, DocSort, MetaPatch, SortField, SortDir, StorageBackend};
use chrono::{DateTime, Utc};
use bson::{doc, Bson, Document as BsonDoc};
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, IndexModel};

/// Kind discriminator inside the `docs` collection.
const KIND_KB: &str = "kb";
const KIND_EPISODIC: &str = "episodic";

/// MongoDB-backed store.
#[derive(Clone)]
pub struct MongoStore {
    db: mongodb::Database,
}

impl MongoStore {
    /// Connect to `uri` (default `mongodb://localhost:27017/slc`); creates
    /// indexes on first use.
    pub async fn connect(uri: Option<&str>) -> SlcResult<Self> {
        let uri = uri
            .map(str::to_string)
            .or_else(|| std::env::var("SLC_MONGODB_URI").ok())
            .unwrap_or_else(|| "mongodb://localhost:27017/slc".into());
        let client = Client::with_uri_str(&uri)
            .await
            .map_err(|e| SlcError::Storage(format!("mongodb connect: {e}")))?;
        let db = client.database("slc");
        let store = Self { db };
        store.ensure_indexes().await?;
        Ok(store)
    }

    /// In-memory MongoDB (ephemeral server; used by tests with mongod).
    pub async fn from_client(client: Client) -> SlcResult<Self> {
        let store = Self {
            db: client.database("slc"),
        };
        store.ensure_indexes().await?;
        Ok(store)
    }

    async fn ensure_indexes(&self) -> SlcResult<()> {
        let docs: Collection<BsonDoc> = self.db.collection("docs");
        docs.create_index(
            IndexModel::builder()
                .keys(doc! { "kind": 1, "seat_id": 1, "category": 1 })
                .options(IndexOptions::builder().name("kind_seat_cat".to_string()).build())
                .build(),
        )
        .await
        .map_err(|e| SlcError::Storage(format!("mongodb index: {e}")))?;
        let embeddings: Collection<BsonDoc> = self.db.collection("embeddings");
        embeddings
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "scope": 1, "seat_id": 1, "document_id": 1 })
                    .options(IndexOptions::builder().name("emb_scope_seat".to_string()).build())
                    .build(),
            )
            .await
            .map_err(|e| SlcError::Storage(format!("mongodb index: {e}")))?;
        Ok(())
    }

    fn docs(&self) -> Collection<BsonDoc> {
        self.db.collection("docs")
    }
    fn embeddings(&self) -> Collection<BsonDoc> {
        self.db.collection("embeddings")
    }
    fn seats(&self) -> Collection<BsonDoc> {
        self.db.collection("seats")
    }
    fn timers(&self) -> Collection<BsonDoc> {
        self.db.collection("timers")
    }
    fn records(&self) -> Collection<BsonDoc> {
        self.db.collection("records")
    }
}

// ── Document ↔ BSON ──────────────────────────────────────────────────────────

/// bson 3 serializes `chrono::DateTime` as an RFC 3339 string; MongoDB
/// sorts/compares real BSON datetimes. Normalize the date fields so stored
/// documents carry proper datetimes (and queries like `created_at >= X`
/// work), and the other way when reading back.
fn normalize_dates(b: &mut BsonDoc, to_datetime: bool) {
    for key in ["created_at", "updated_at", "deleted_at"] {
        match (to_datetime, b.get(key)) {
            (true, Some(Bson::String(s))) => {
                if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                    b.insert(key, Bson::DateTime(dt.with_timezone(&Utc).into()));
                }
            }
            (false, Some(Bson::DateTime(d))) => {
                use chrono::SecondsFormat;
                b.insert(
                    key,
                    Bson::String(d.to_chrono().to_rfc3339_opts(SecondsFormat::AutoSi, true)),
                );
            }
            _ => {}
        }
    }
}

fn doc_to_bson(kind: &str, doc: &Document) -> SlcResult<BsonDoc> {
    let mut b = bson::serialize_to_document(doc)
        .map_err(|e| SlcError::Storage(format!("doc to bson: {e}")))?;
    normalize_dates(&mut b, true);
    b.insert("_id", Bson::String(doc.document_id.clone()));
    b.insert("kind", Bson::String(kind.into()));
    Ok(b)
}

fn bson_to_doc(b: BsonDoc) -> SlcResult<Document> {
    // _id is authoritative for the id (documents may be upserted by name).
    let mut b = b;
    let id = b
        .get("_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    b.insert("document_id", Bson::String(id));
    b.remove("_id");
    b.remove("kind");
    normalize_dates(&mut b, false);
    bson::deserialize_from_document::<Document>(b).map_err(|e| SlcError::Storage(format!("bson to doc: {e}")))
}

/// Build the query for a `DocFilter` plus the kind discriminator.
fn filter_query(kind: &str, f: &DocFilter) -> BsonDoc {
    let mut q = doc! { "kind": kind };
    if let Some(cat) = f.category {
        q.insert("category", Bson::String(cat.as_str().into()));
    }
    if let Some(seat) = &f.seat_id {
        q.insert("seat_id", Bson::String(seat.clone()));
    }
    if let Some(vis) = &f.visible_to {
        q.insert(
            "seat_id",
            doc! { "$in": [Bson::Null, Bson::String(vis.clone())] },
        );
    }
    if let Some(level) = f.doc_level {
        let v = bson::serialize_to_bson(&level).unwrap_or(Bson::Null);
        q.insert("metadata.doc_level", v);
    }
    if let Some(t) = &f.doc_type {
        q.insert("metadata.doc_type", Bson::String(t.clone()));
    }
    if let Some(a) = f.archived {
        q.insert("metadata.archived", Bson::Boolean(a));
    }
    if f.not_consolidated {
        q.insert("metadata.consolidated", doc! { "$ne": true });
    }
    if let Some(h) = f.has_compression_batch {
        q.insert("metadata.compression_batch_id", doc! { "$exists": h });
    }
    if !f.tags_any.is_empty() || !f.tags_all.is_empty() {
        let mut tags = BsonDoc::new();
        if !f.tags_any.is_empty() {
            tags.insert("$in", Bson::Array(f.tags_any.iter().map(|t| Bson::String(t.clone())).collect()));
        }
        if !f.tags_all.is_empty() {
            tags.insert("$all", Bson::Array(f.tags_all.iter().map(|t| Bson::String(t.clone())).collect()));
        }
        q.insert("tags", Bson::Document(tags));
    }
    // `{field: null}` matches both missing and explicitly-null fields —
    // serde emits `deleted_at: null` for not-deleted documents.
    if f.deleted {
        q.insert("deleted_at", doc! { "$exists": true });
    } else {
        q.insert("deleted_at", Bson::Null);
    }
    if let Some(since) = f.since {
        q.insert("created_at", doc! { "$gte": since });
    }
    if let Some(until) = f.until {
        q.insert("created_at", doc! { "$lte": until });
    }
    q
}

fn sort_doc(sort: &DocSort) -> BsonDoc {
    let (field, dir) = match sort.field {
        SortField::CreatedAt => ("created_at", sort.dir),
        SortField::UpdatedAt => ("updated_at", sort.dir),
    };
    doc! { field: if dir == SortDir::Asc { 1i32 } else { -1i32 } }
}

fn meta_patch_doc(patch: &MetaPatch) -> BsonDoc {
    let mut set = doc! { "updated_at": Utc::now() };
    if let Some(v) = patch.set_archived {
        set.insert("metadata.archived", Bson::Boolean(v));
    }
    if let Some(v) = patch.set_consolidated {
        set.insert("metadata.consolidated", Bson::Boolean(v));
    }
    if let Some(v) = &patch.set_compression_batch_id {
        set.insert("metadata.compression_batch_id", Bson::String(v.clone()));
    }
    doc! { "$set": set }
}

// ── StorageBackend ───────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl StorageBackend for MongoStore {
    // ── KB ──
    async fn kb_insert(&self, doc: &Document) -> SlcResult<()> {
        self.docs()
            .insert_one(doc_to_bson(KIND_KB, doc)?)
            .await
            .map_err(|e| SlcError::Storage(format!("kb insert: {e}")))?;
        Ok(())
    }

    async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>> {
        let found = self
            .docs()
            .find_one(doc! { "_id": document_id, "kind": KIND_KB })
            .await
            .map_err(|e| SlcError::Storage(format!("kb get: {e}")))?;
        found.map(bson_to_doc).transpose()
    }

    async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool> {
        let res = self
            .docs()
            .update_one(doc! { "_id": document_id, "kind": KIND_KB },
                doc! { "$set": { "content": content, "updated_at": Utc::now() } })
            .await
            .map_err(|e| SlcError::Storage(format!("kb update content: {e}")))?;
        Ok(res.modified_count > 0)
    }

    async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool> {
        let existing = self.kb_get(&doc.document_id).await?;
        if existing.is_some() {
            return self.kb_update_content(&doc.document_id, &doc.content).await;
        }
        self.kb_insert(doc).await?;
        Ok(false)
    }

    async fn kb_replace(&self, doc: &Document) -> SlcResult<bool> {
        let existed = self.kb_get(&doc.document_id).await?.is_some();
        self.docs()
            .replace_one(doc! { "_id": &doc.document_id, "kind": KIND_KB },
                doc_to_bson(KIND_KB, doc)?).upsert(true)
            .await
            .map_err(|e| SlcError::Storage(format!("kb replace: {e}")))?;
        Ok(existed)
    }

    async fn kb_find(&self, filter: &DocFilter, sort: &DocSort, limit: usize) -> SlcResult<Vec<Document>> {
        let mut cursor = self
            .docs()
            .find(filter_query(KIND_KB, filter))
            .sort(sort_doc(sort))
            .limit(limit as i64)
            .await
            .map_err(|e| SlcError::Storage(format!("kb find: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            out.push(bson_to_doc(b)?);
        }
        Ok(out)
    }

    async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        self.docs()
            .count_documents(filter_query(KIND_KB, filter))
            .await
            .map_err(|e| SlcError::Storage(format!("kb count: {e}")))
    }

    async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        let res = self
            .docs()
            .update_many(filter_query(KIND_KB, filter), meta_patch_doc(patch))
            .await
            .map_err(|e| SlcError::Storage(format!("kb patch meta: {e}")))?;
        Ok(res.modified_count as usize)
    }

    async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool> {
        let res = self
            .docs()
            .update_one(doc! { "_id": document_id, "kind": KIND_KB },
                doc! { "$set": { "deleted_at": Utc::now() } })
            .await
            .map_err(|e| SlcError::Storage(format!("kb soft delete: {e}")))?;
        Ok(res.modified_count > 0)
    }

    async fn kb_restore(&self, document_id: &str) -> SlcResult<bool> {
        let res = self
            .docs()
            .update_one(doc! { "_id": document_id, "kind": KIND_KB },
                doc! { "$unset": { "deleted_at": "" } })
            .await
            .map_err(|e| SlcError::Storage(format!("kb restore: {e}")))?;
        Ok(res.modified_count > 0)
    }

    async fn kb_purge(&self, document_id: &str) -> SlcResult<bool> {
        let res = self
            .docs()
            .delete_one(doc! { "_id": document_id, "kind": KIND_KB })
            .await
            .map_err(|e| SlcError::Storage(format!("kb purge: {e}")))?;
        Ok(res.deleted_count > 0)
    }

    async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>> {
        let cutoff = Utc::now() - chrono::Duration::days(days.unwrap_or(30));
        let q = doc! { "kind": KIND_KB, "deleted_at": { "$lte": cutoff } };
        let mut cursor = self
            .docs()
            .find(q)
            .await
            .map_err(|e| SlcError::Storage(format!("kb graveyard: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            out.push(bson_to_doc(b)?);
        }
        Ok(out)
    }

    async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64> {
        let cutoff = Utc::now() - chrono::Duration::days(days);
        let res = self
            .docs()
            .delete_many(doc! { "kind": KIND_KB, "deleted_at": { "$lte": cutoff } })
            .await
            .map_err(|e| SlcError::Storage(format!("kb cleanup: {e}")))?;
        Ok(res.deleted_count)
    }

    // ── Episodic ──
    async fn episodic_insert(&self, doc: &Document) -> SlcResult<()> {
        self.docs()
            .insert_one(doc_to_bson(KIND_EPISODIC, doc)?)
            .await
            .map_err(|e| SlcError::Storage(format!("episodic insert: {e}")))?;
        Ok(())
    }

    async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool> {
        let existing: Option<BsonDoc> = self
            .docs()
            .find_one(doc! { "_id": &doc.document_id, "kind": KIND_EPISODIC })
            .await
            .map_err(|e| SlcError::Storage(format!("episodic get: {e}")))?;
        let existed = existing.is_some();
        self.docs()
            .replace_one(doc! { "_id": &doc.document_id, "kind": KIND_EPISODIC },
                doc_to_bson(KIND_EPISODIC, doc)?).upsert(true)
            .await
            .map_err(|e| SlcError::Storage(format!("episodic upsert: {e}")))?;
        Ok(existed)
    }

    async fn episodic_find(&self, filter: &DocFilter, sort: &DocSort, limit: usize) -> SlcResult<Vec<Document>> {
        let mut cursor = self
            .docs()
            .find(filter_query(KIND_EPISODIC, filter))
            .sort(sort_doc(sort))
            .limit(limit as i64)
            .await
            .map_err(|e| SlcError::Storage(format!("episodic find: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            out.push(bson_to_doc(b)?);
        }
        Ok(out)
    }

    async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        self.docs()
            .count_documents(filter_query(KIND_EPISODIC, filter))
            .await
            .map_err(|e| SlcError::Storage(format!("episodic count: {e}")))
    }

    async fn episodic_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        let res = self
            .docs()
            .update_many(filter_query(KIND_EPISODIC, filter), meta_patch_doc(patch))
            .await
            .map_err(|e| SlcError::Storage(format!("episodic patch meta: {e}")))?;
        Ok(res.modified_count as usize)
    }

    async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool> {
        let res = self
            .docs()
            .delete_one(doc! { "_id": document_id, "kind": KIND_EPISODIC })
            .await
            .map_err(|e| SlcError::Storage(format!("episodic purge: {e}")))?;
        Ok(res.deleted_count > 0)
    }

    // ── Embeddings ──
    async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()> {
        let docs: Vec<BsonDoc> = records
            .iter()
            .map(|r| {
                let mut b = bson::serialize_to_document(r)
                    .map_err(|e| SlcError::Storage(format!("embedding to bson: {e}")))?;
                b.insert(
                    "_id",
                    Bson::String(format!("{}#{}", r.document_id, r.chunk_index)),
                );
                Ok(b)
            })
            .collect::<SlcResult<Vec<_>>>()?;
        if !docs.is_empty() {
            self.embeddings()
                .insert_many(docs)
                .await
                .map_err(|e| SlcError::Storage(format!("insert embeddings: {e}")))?;
        }
        Ok(())
    }

    async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>> {
        let mut cursor = self
            .embeddings()
            .find(doc! { "document_id": document_id })
            .sort(doc! { "chunk_index": 1i32 })
            .await
            .map_err(|e| SlcError::Storage(format!("get embedding: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            if let Ok(arr) = b.get_array("embedding") {
                out.extend(arr.iter().filter_map(|v| v.as_f64()).map(|v| v as f32));
            }
        }
        Ok(if out.is_empty() { None } else { Some(out) })
    }

    async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>> {
        let mut cursor = self
            .embeddings()
            .find(doc! { "document_id": document_id })
            .sort(doc! { "chunk_index": 1i32 })
            .await
            .map_err(|e| SlcError::Storage(format!("get chunks: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            out.push(bson::deserialize_from_document::<EmbeddingRecord>(b).map_err(|e| SlcError::Storage(format!("row: {e}")))?);
        }
        Ok(out)
    }

    async fn all_embeddings(&self, scope: EmbeddingScope, seat_id: Option<&str>) -> SlcResult<Vec<EmbeddingRecord>> {
        let scope_v = bson::serialize_to_bson(&scope).unwrap_or(Bson::Null);
        let mut q = doc! { "scope": scope_v };
        match seat_id {
            Some(s) => {
                q.insert("seat_id", Bson::String(s.into()));
            }
            None => {
                q.insert("seat_id", Bson::Null);
            }
        }
        let mut cursor = self
            .embeddings()
            .find(q)
            .await
            .map_err(|e| SlcError::Storage(format!("all embeddings: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            out.push(bson::deserialize_from_document::<EmbeddingRecord>(b).map_err(|e| SlcError::Storage(format!("row: {e}")))?);
        }
        Ok(out)
    }

    async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()> {
        self.embeddings()
            .delete_many(doc! { "document_id": document_id })
            .await
            .map_err(|e| SlcError::Storage(format!("delete embeddings: {e}")))?;
        Ok(())
    }

    // ── Seats ──
    async fn insert_seat(&self, seat: &Seat) -> SlcResult<()> {
        let mut b = bson::serialize_to_document(seat)
            .map_err(|e| SlcError::Storage(format!("seat to bson: {e}")))?;
        b.insert("_id", Bson::String(seat.seat_id.clone()));
        self.seats()
            .replace_one(doc! { "_id": &seat.seat_id }, b).upsert(true)
            .await
            .map_err(|e| SlcError::Storage(format!("seat upsert: {e}")))?;
        Ok(())
    }

    async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
        let found = self
            .seats()
            .find_one(doc! { "_id": seat_id })
            .await
            .map_err(|e| SlcError::Storage(format!("get seat: {e}")))?;
        found
            .map(|b| {
                bson::deserialize_from_document::<Seat>(b).map_err(|e| SlcError::Storage(format!("seat from bson: {e}")))
            })
            .transpose()
    }

    async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>> {
        let mut cursor = self
            .seats()
            .find(doc! { "status": "active" })
            .sort(doc! { "last_accessed": -1i32 })
            .limit(limit as i64)
            .await
            .map_err(|e| SlcError::Storage(format!("list seats: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            out.push(bson::deserialize_from_document::<Seat>(b).map_err(|e| SlcError::Storage(format!("row: {e}")))?);
        }
        Ok(out)
    }

    async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool> {
        let res = self
            .seats()
            .update_one(doc! { "_id": seat_id },
                doc! { "$set": { "last_accessed": Utc::now() } })
            .await
            .map_err(|e| SlcError::Storage(format!("touch seat: {e}")))?;
        Ok(res.modified_count > 0)
    }

    async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool> {
        let res = self
            .seats()
            .update_one(doc! { "_id": seat_id },
                doc! { "$set": { "status": status.as_str() } })
            .await
            .map_err(|e| SlcError::Storage(format!("set seat status: {e}")))?;
        Ok(res.modified_count > 0)
    }

    async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let res = self
            .seats()
            .update_one(doc! { "_id": seat_id },
                doc! { "$set": { "active_task_id": task_id, "active_document_id": task_id } })
            .await
            .map_err(|e| SlcError::Storage(format!("set active task: {e}")))?;
        Ok(res.modified_count > 0)
    }

    async fn set_seat_active_document(&self, seat_id: &str, document_id: Option<&str>) -> SlcResult<bool> {
        let value = document_id.map(|s| Bson::String(s.to_string())).unwrap_or(Bson::Null);
        let res = self
            .seats()
            .update_one(doc! { "_id": seat_id }, doc! { "$set": { "active_document_id": value } })
            .await
            .map_err(|e| SlcError::Storage(format!("set active document: {e}")))?;
        Ok(res.modified_count > 0)
    }

    async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
        let found = self
            .seats()
            .find_one(doc! { "_id": seat_id }).projection(doc! { "active_document_id": 1i32, "active_task_id": 1i32 })
            .await
            .map_err(|e| SlcError::Storage(format!("get active document: {e}")))?;
        Ok(found.and_then(|b| {
            b.get_str("active_document_id")
                .ok()
                .map(str::to_string)
                .or_else(|| b.get_str("active_task_id").ok().map(str::to_string))
        }))
    }

    async fn incr_seat_stats(&self, seat_id: &str, tool_name: &str, tokens_used: i64) -> SlcResult<bool> {
        let res = self
            .seats()
            .update_one(doc! { "_id": seat_id },
                doc! { "$inc": {
                    "usage_stats.total_requests": 1i64,
                    "usage_stats.total_tokens": tokens_used,
                    format!("usage_stats.tools_used.{}", tool_name): 1i64,
                }})
            .await
            .map_err(|e| SlcError::Storage(format!("incr stats: {e}")))?;
        Ok(res.modified_count > 0)
    }

    // ── Timers ──
    async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
        let mut b = bson::serialize_to_document(timer)
            .map_err(|e| SlcError::Storage(format!("timer to bson: {e}")))?;
        b.insert("_id", Bson::String(timer.timer_id.clone()));
        self.timers()
            .replace_one(doc! { "_id": &timer.timer_id }, b).upsert(true)
            .await
            .map_err(|e| SlcError::Storage(format!("timer upsert: {e}")))?;
        Ok(())
    }

    async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>> {
        let found = self
            .timers()
            .find_one(doc! { "_id": timer_id })
            .await
            .map_err(|e| SlcError::Storage(format!("get timer: {e}")))?;
        found
            .map(|b| {
                bson::deserialize_from_document::<PersistedTimer>(b)
                    .map_err(|e| SlcError::Storage(format!("timer from bson: {e}")))
            })
            .transpose()
    }

    async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
        let mut q = doc! { "is_active": true };
        if let Some(s) = seat_id {
            q.insert("seat_id", Bson::String(s.into()));
        }
        let mut cursor = self
            .timers()
            .find(q)
            .await
            .map_err(|e| SlcError::Storage(format!("active timers: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            out.push(bson::deserialize_from_document::<PersistedTimer>(b).map_err(|e| SlcError::Storage(format!("row: {e}")))?);
        }
        Ok(out)
    }

    async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()> {
        self.timers()
            .update_one(doc! { "_id": timer_id }, doc! { "$set": { "last_fired_at": now } })
            .await
            .map_err(|e| SlcError::Storage(format!("set timer fired: {e}")))?;
        Ok(())
    }

    // ── Records ──
    async fn put_record(&self, collection: &str, key: &str, value: &serde_json::Value) -> SlcResult<()> {
        let bson_val = bson::serialize_to_bson(value)
            .map_err(|e| SlcError::Storage(format!("record to bson: {e}")))?;
        self.records()
            .replace_one(doc! { "_id": format!("{collection}/{key}") }, doc! { "_id": format!("{collection}/{key}"), "collection": collection, "key": key, "value": bson_val }).upsert(true)
            .await
            .map_err(|e| SlcError::Storage(format!("put record: {e}")))?;
        Ok(())
    }

    async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<serde_json::Value>> {
        let found = self
            .records()
            .find_one(doc! { "_id": format!("{collection}/{key}") })
            .await
            .map_err(|e| SlcError::Storage(format!("get record: {e}")))?;
        found
            .map(|b| {
                b.get("value")
                    .cloned()
                    .map(|v| bson::deserialize_from_bson::<serde_json::Value>(v).map_err(|e| SlcError::Storage(format!("record value: {e}"))))
                    .unwrap_or_else(|| Ok(serde_json::Value::Null))
            })
            .transpose()
    }

    async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool> {
        let res = self
            .records()
            .delete_one(doc! { "_id": format!("{collection}/{key}") })
            .await
            .map_err(|e| SlcError::Storage(format!("delete record: {e}")))?;
        Ok(res.deleted_count > 0)
    }

    async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, serde_json::Value)>> {
        let mut cursor = self
            .records()
            .find(doc! { "collection": collection })
            .await
            .map_err(|e| SlcError::Storage(format!("list records: {e}")))?;
        let mut out = Vec::new();
        while cursor.advance().await.map_err(|e| SlcError::Storage(format!("cursor: {e}")))? {
            let b: BsonDoc = cursor.deserialize_current().map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            if let (Some(key), Some(val)) = (b.get_str("key").ok(), b.get("value")) {
                if let Ok(v) = bson::deserialize_from_bson::<serde_json::Value>(val.clone()) {
                    out.push((key.to_string(), v));
                }
            }
        }
        Ok(out)
    }

    // ── health / close ──
    async fn health_check(&self) -> bool {
        self.db
            .run_command(doc! { "ping": 1i32 })
            .await
            .is_ok()
    }

    async fn close(&self) -> SlcResult<()> {
        Ok(())
    }
}

// ── tests ────────────────────────────────────────────────────────────────────
// Roundtrip/BSON tests run without a server; integration tests need a live
// mongod (SLC_MONGODB_URI set or localhost:27017) and are marked #[ignore].

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Document, DocumentCategory};

    fn sample_doc() -> Document {
        Document::new(
            "project_vassista",
            DocumentCategory::Project,
            "Голосовая платформа на Rust",
            Default::default(),
            vec!["priority:high".into()],
            Some("seat_x".into()),
        )
    }

    #[test]
    fn document_bson_roundtrip() {
        let doc = sample_doc();
        let b = doc_to_bson(KIND_KB, &doc).unwrap();
        assert_eq!(b.get_str("_id").unwrap(), "project_vassista");
        assert_eq!(b.get_str("kind").unwrap(), KIND_KB);
        let back = bson_to_doc(b).unwrap();
        assert_eq!(back.document_id, doc.document_id);
        assert_eq!(back.category, doc.category);
        assert_eq!(back.content, doc.content);
        assert_eq!(back.tags, doc.tags);
        assert_eq!(back.seat_id, doc.seat_id);
        // BSON datetimes are millisecond-precision.
        assert_eq!(back.created_at.timestamp_millis(), doc.created_at.timestamp_millis());
    }

    #[test]
    fn filter_query_maps_operators() {
        let mut f = DocFilter::default();
        f.category = Some(DocumentCategory::Skill);
        f.tags_any = vec!["a".into(), "b".into()];
        f.tags_all = vec!["c".into()];
        f.not_consolidated = true;
        f.deleted = false;
        let q = filter_query(KIND_KB, &f);
        assert_eq!(q.get_str("kind").unwrap(), KIND_KB);
        assert_eq!(q.get_str("category").unwrap(), "skill");
        let tags = q.get("tags").unwrap().as_document().unwrap();
        assert_eq!(tags.get_array("$in").unwrap().len(), 2);
        assert_eq!(tags.get_array("$all").unwrap().len(), 1);
        assert!(q.get("deleted_at").unwrap().as_null().is_some(), "deleted=false → null filter");
    }

    #[test]
    fn meta_patch_builds_set() {
        let p = MetaPatch {
            set_archived: Some(true),
            set_consolidated: Some(true),
            set_compression_batch_id: Some("b1".into()),
        };
        let d = meta_patch_doc(&p);
        let set = d.get_document("$set").unwrap();
        assert_eq!(set.get_bool("metadata.archived").unwrap(), true);
        assert_eq!(set.get_bool("metadata.consolidated").unwrap(), true);
        assert_eq!(set.get_str("metadata.compression_batch_id").unwrap(), "b1");
    }

    /// Full CRUD against a live mongod. Run manually:
    /// `mongod --dbpath /tmp/slc-mongo --fork --logpath /tmp/slc-mongo.log`
    #[tokio::test]
    #[ignore = "requires a running mongod"]
    async fn mongo_crud_roundtrip() {
        let store = MongoStore::connect(None).await.unwrap();
        assert!(store.health_check().await);
        // Idempotent: start from a clean database.
        store.db.drop().await.unwrap();

        // KB CRUD.
        let doc = sample_doc();
        store.kb_insert(&doc).await.unwrap();
        let got = store.kb_get("project_vassista").await.unwrap().unwrap();
        assert_eq!(got.content, doc.content);
        assert_eq!(got.seat_id.as_deref(), Some("seat_x"));

        assert!(store.kb_update_content("project_vassista", "новый контент").await.unwrap());
        let got = store.kb_get("project_vassista").await.unwrap().unwrap();
        assert_eq!(got.content, "новый контент");

        assert_eq!(store.kb_count(&DocFilter::default()).await.unwrap(), 1);
        let hits = store.kb_find(&DocFilter::default(), &DocSort::by_created(SortDir::Desc), 10).await.unwrap();
        assert_eq!(hits.len(), 1);

        // Episodic is a separate kind.
        let hist = Document::new(
            "ev_1",
            DocumentCategory::History,
            "работали над mongo",
            Default::default(),
            vec![],
            Some("seat_x".into()),
        );
        store.episodic_insert(&hist).await.unwrap();
        assert_eq!(store.kb_count(&DocFilter::default()).await.unwrap(), 1);
        assert_eq!(store.episodic_count(&DocFilter::default()).await.unwrap(), 1);

        // Seats.
        let seat = crate::model::Seat {
            seat_id: "seat_x".into(),
            name: "x".into(),
            status: crate::model::SeatStatus::Active,
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
        assert!(store.set_seat_active_document("seat_x", Some("project_vassista")).await.unwrap());
        assert_eq!(
            store.get_seat_active_document("seat_x").await.unwrap().as_deref(),
            Some("project_vassista")
        );
        assert_eq!(store.list_active_seats(10).await.unwrap().len(), 1);
        assert!(store.incr_seat_stats("seat_x", "search", 12).await.unwrap());

        // Records.
        store.put_record("notifications", "n1", &serde_json::json!({"text": "hi"})).await.unwrap();
        assert_eq!(store.list_records("notifications").await.unwrap().len(), 1);
        assert!(store.delete_record("notifications", "n1").await.unwrap());

        // Timers.
        let timer = crate::model::PersistedTimer {
            timer_id: "t1".into(),
            seat_id: "seat_x".into(),
            timer_type: crate::model::TimerType::HistoryCompression,
            interval_seconds: Some(3600),
            last_fired_at: None,
            next_fire_at: Utc::now(),
            is_active: true,
            metadata: Default::default(),
            created_at: Utc::now(),
        };
        store.insert_timer(&timer).await.unwrap();
        assert_eq!(store.active_timers(Some("seat_x")).await.unwrap().len(), 1);

        // Embeddings.
        let rec = crate::model::EmbeddingRecord {
            document_id: "project_vassista".into(),
            chunk_index: 0,
            chunk_total: 1,
            embedding: vec![0.1, 0.2, 0.3],
            embedding_model: "bge-m3".into(),
            embedding_dimension: 3,
            generated_at: Utc::now(),
            scope: crate::model::EmbeddingScope::Private,
            seat_id: Some("seat_x".into()),
        };
        store.insert_embeddings(&[rec]).await.unwrap();
        let emb = store.get_embedding("project_vassista").await.unwrap().unwrap();
        assert_eq!(emb.len(), 3);
        assert_eq!(store.get_all_chunks("project_vassista").await.unwrap().len(), 1);

        // Cleanup.
        assert!(store.kb_purge("project_vassista").await.unwrap());
        assert!(store.episodic_purge("ev_1").await.unwrap());
        assert!(store.kb_get("project_vassista").await.unwrap().is_none());
    }
}
