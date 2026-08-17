//! Obsidian vault backend — the DEFAULT storage for SLC.
//!
//! A vault is a directory of human-readable markdown notes organized in
//! folders (Obsidian is a folder-based vault). Layout:
//!
//! ```text
//! vault/
//!   core/ modules/ tasks/ projects/ docs/ code/ custom/ system/
//!     <document_id>.md            ← KB documents, human-readable names
//!   history/YYYY/MM/<document_id>.md   ← episodic (diary layout by date)
//!   seats/<seat_id>.json          ← seats (operational data)
//!   .slc/
//!     index.json                  ← fast document index (id → path + filter fields)
//!     embeddings.json             ← embedding chunks
//!     timers.json                 ← persisted timers
//!     records.json                ← free-form JSON records
//! ```
//!
//! Each note: YAML frontmatter (all fields except `content`) + markdown body
//! (the document content). Files are the source of truth — the index is
//! rebuilt on open so hand-edits in Obsidian are picked up. Optional git
//! auto-commit keeps the vault versioned (default off).
//!
//! KB and episodic live in separate folder trees (history never gets
//! embedded/searched), mirroring the [`StorageBackend`] split.

use super::{DocFilter, DocSort, MetaPatch, SortDir, StorageBackend};
use crate::error::{SlcError, SlcResult};
use crate::model::{
    content_hash, Document, DocumentCategory, EmbeddingRecord, EmbeddingScope,
    PersistedTimer, Seat, SeatStatus,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Human-readable file name from a unique document id.
/// Keeps letters/digits/`-`/`_`/`.`/space; path-hostile chars → `-`.
/// Validate a vault folder path from an untrusted document. Nested
/// relative paths are fine (`projects/vassista`, `history/2026/08` —
/// `default_folder` builds those from categories and ids); anything that
/// could escape the vault root is rejected: absolute paths, `..`
/// components, backslashes, colons and control characters.
pub fn sanitize_folder(folder: &str) -> SlcResult<String> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err(SlcError::Storage("folder must not be empty".into()));
    }
    if folder.starts_with('/') {
        return Err(SlcError::Storage(format!("folder must be relative to the vault: {folder}")));
    }
    for part in folder.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(SlcError::Storage(format!("folder contains an invalid component: {folder}")));
        }
        if part
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':'))
        {
            return Err(SlcError::Storage(format!("folder contains invalid characters: {folder}")));
        }
    }
    Ok(folder.to_string())
}

pub fn safe_file_name(document_id: &str) -> String {
    let mut out = String::with_capacity(document_id.len());
    for c in document_id.chars() {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("doc");
    }
    out
}

// ─────────────────────────────── index ───────────────────────────────

/// Filter fields for one document (mirrors `DocFilter`; source of truth for
/// fast listing/counting without reading note bodies).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub folder: String,
    pub category: String,
    pub seat_id: Option<String>,
    pub doc_level: Option<String>,
    pub doc_type: Option<String>,
    pub archived: Option<bool>,
    pub consolidated: Option<bool>,
    pub compression_batch_id: Option<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub version: i64,
    pub deleted_at: Option<String>,
}

impl IndexEntry {
    fn from_doc(doc: &Document) -> Self {
        IndexEntry {
            id: doc.document_id.clone(),
            folder: doc.folder.clone().unwrap_or_else(|| doc.default_folder()),
            category: doc.category.as_str().to_string(),
            seat_id: doc.seat_id.clone(),
            doc_level: doc.metadata.doc_level.map(|l| l.as_str().to_string()),
            doc_type: doc.metadata.doc_type.clone(),
            archived: doc.metadata.archived,
            consolidated: doc.metadata.consolidated,
            compression_batch_id: doc.metadata.compression_batch_id.clone(),
            tags: doc.tags.clone(),
            created_at: doc.created_at.to_rfc3339(),
            updated_at: doc.updated_at.to_rfc3339(),
            version: doc.version,
            deleted_at: doc.deleted_at.map(|d| d.to_rfc3339()),
        }
    }

    fn matches(&self, f: &DocFilter) -> bool {
        if let Some(cat) = f.category {
            if self.category != cat.as_str() {
                return false;
            }
        }
        if let Some(seat) = &f.seat_id {
            if self.seat_id.as_deref() != Some(seat.as_str()) {
                return false;
            }
        }
        if let Some(vis) = &f.visible_to {
            if let Some(owner) = &self.seat_id {
                if owner != vis {
                    return false;
                }
            }
        }
        if let Some(level) = f.doc_level {
            if self.doc_level.as_deref() != Some(level.as_str()) {
                return false;
            }
        }
        if let Some(ids) = &f.document_ids {
            if !ids.iter().any(|id| id == &self.id) {
                return false;
            }
        }
        if let Some(dt) = &f.doc_type {
            if self.doc_type.as_deref() != Some(dt.as_str()) {
                return false;
            }
        }
        if let Some(archived) = f.archived {
            if self.archived != Some(archived) {
                return false;
            }
        }
        if f.not_consolidated && self.consolidated == Some(true) {
            return false;
        }
        match f.has_compression_batch {
            Some(true) => {
                if self.compression_batch_id.is_none() {
                    return false;
                }
            }
            Some(false) => {
                if self.compression_batch_id.is_some() {
                    return false;
                }
            }
            None => {}
        }
        if !f.tags_any.is_empty() && !f.tags_any.iter().any(|t| self.tags.contains(t)) {
            return false;
        }
        if !f.tags_all.is_empty() && !f.tags_all.iter().all(|t| self.tags.contains(t)) {
            return false;
        }
        if f.deleted {
            if self.deleted_at.is_none() {
                return false;
            }
        } else if self.deleted_at.is_some() {
            return false;
        }
        let updated = self.updated_at.parse::<DateTime<Utc>>().ok();
        if let (Some(since), Some(u)) = (f.since, updated) {
            if u < since {
                return false;
            }
        }
        if let (Some(until), Some(u)) = (f.until, updated) {
            if u > until {
                return false;
            }
        }
        true
    }
}

// ─────────────────────────────── store ───────────────────────────────

/// Obsidian vault backend — DEFAULT. See module docs for layout.
#[derive(Clone)]
pub struct ObsidianVaultStore {
    root: PathBuf,
    index: std::sync::Arc<Mutex<HashMap<String, IndexEntry>>>,
    embeddings: std::sync::Arc<Mutex<HashMap<String, Vec<EmbeddingRecord>>>>,
    seats: std::sync::Arc<Mutex<HashMap<String, Seat>>>,
    timers: std::sync::Arc<Mutex<HashMap<String, PersistedTimer>>>,
    records: std::sync::Arc<Mutex<HashMap<(String, String), Value>>>,
    auto_git_commit: bool,
    git_author: String,
    git_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

impl ObsidianVaultStore {
    /// Open a vault directory (created if missing). `auto_git_commit` runs
    /// `git add -A && git commit` after every write (vault must be a repo).
    pub fn open(root: impl AsRef<Path>, auto_git_commit: bool) -> SlcResult<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join(".slc"))?;
        std::fs::create_dir_all(root.join("seats"))?;
        let store = ObsidianVaultStore {
            root,
            index: std::sync::Arc::new(Mutex::new(HashMap::new())),
            embeddings: std::sync::Arc::new(Mutex::new(HashMap::new())),
            seats: std::sync::Arc::new(Mutex::new(HashMap::new())),
            timers: std::sync::Arc::new(Mutex::new(HashMap::new())),
            records: std::sync::Arc::new(Mutex::new(HashMap::new())),
            auto_git_commit,
            git_author: std::env::var("OBSIDIAN_GIT_AUTHOR")
                .unwrap_or_else(|_| "slc-mcp <slc-mcp@local>".into()),
            git_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        };
        store.rebuild_index()?;
        store.load_sidecars()?;
        Ok(store)
    }

    fn slc_dir(&self) -> PathBuf {
        self.root.join(".slc")
    }

    /// Scan the vault and rebuild the document index from file frontmatter
    /// (picks up hand-edits made in Obsidian).
    pub fn rebuild_index(&self) -> SlcResult<()> {
        let mut index = HashMap::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)?;
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    let name = e.file_name();
                    let name = name.to_string_lossy().to_string();
                    if name == ".git" || name == ".slc" || name == "seats" {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "md") {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let (meta, _) = frontmatter_parse(&text);
                    if let Some(meta) = meta {
                        if let Some(id) = meta.get("id").and_then(|v| v.as_str()) {
                            let rel = path
                                .strip_prefix(&self.root)
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_default();
                            let entry = IndexEntry {
                                id: id.to_string(),
                                folder: Path::new(&rel)
                                    .parent()
                                    .map(|p| p.to_string_lossy().to_string())
                                    .unwrap_or_default(),
                                category: meta
                                    .get("category")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("custom")
                                    .to_string(),
                                seat_id: meta.get("seat_id").and_then(|v| v.as_str()).map(String::from),
                                doc_level: meta
                                    .get("metadata")
                                    .and_then(|m| m.get("doc_level"))
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                doc_type: meta
                                    .get("metadata")
                                    .and_then(|m| m.get("doc_type"))
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                archived: meta
                                    .get("metadata")
                                    .and_then(|m| m.get("archived"))
                                    .and_then(|v| v.as_bool()),
                                consolidated: meta
                                    .get("metadata")
                                    .and_then(|m| m.get("consolidated"))
                                    .and_then(|v| v.as_bool()),
                                compression_batch_id: meta
                                    .get("metadata")
                                    .and_then(|m| m.get("compression_batch_id"))
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                tags: meta
                                    .get("tags")
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter().filter_map(|v| v.as_str().map(String::from)).collect()
                                    })
                                    .unwrap_or_default(),
                                created_at: meta
                                    .get("created_at")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                updated_at: meta
                                    .get("updated_at")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                version: meta.get("version").and_then(|v| v.as_i64()).unwrap_or(1),
                                deleted_at: meta
                                    .get("deleted_at")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                            };
                            index.insert(id.to_string(), entry);
                        }
                    }
                }
            }
        }
        *self.index.lock().unwrap() = index;
        self.persist_index()
    }

    fn load_sidecars(&self) -> SlcResult<()> {
        let emb_path = self.slc_dir().join("embeddings.json");
        if let Ok(text) = std::fs::read_to_string(&emb_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, Vec<EmbeddingRecord>>>(&text) {
                *self.embeddings.lock().unwrap() = map;
            }
        }
        let timers_path = self.slc_dir().join("timers.json");
        if let Ok(text) = std::fs::read_to_string(&timers_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, PersistedTimer>>(&text) {
                *self.timers.lock().unwrap() = map;
            }
        }
        let records_path = self.slc_dir().join("records.json");
        if let Ok(text) = std::fs::read_to_string(&records_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, Value>>(&text) {
                let map: HashMap<(String, String), Value> = map
                    .into_iter()
                    .filter_map(|(k, v)| {
                        let (coll, key) = k.split_once('\u{0}')?;
                        Some(((coll.to_string(), key.to_string()), v))
                    })
                    .collect();
                *self.records.lock().unwrap() = map;
            }
        }
        // Seats: one JSON file per seat in seats/.
        let mut seats = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(self.root.join("seats")) {
            for e in rd.flatten() {
                if let Ok(text) = std::fs::read_to_string(e.path()) {
                    if let Ok(seat) = serde_json::from_str::<Seat>(&text) {
                        seats.insert(seat.seat_id.clone(), seat);
                    }
                }
            }
        }
        *self.seats.lock().unwrap() = seats;
        Ok(())
    }

    fn persist_index(&self) -> SlcResult<()> {
        let map = self.index.lock().unwrap();
        let text = serde_json::to_string_pretty(&*map)?;
        std::fs::write(self.slc_dir().join("index.json"), text)?;
        Ok(())
    }

    fn persist_embeddings(&self) -> SlcResult<()> {
        let map = self.embeddings.lock().unwrap();
        std::fs::write(
            self.slc_dir().join("embeddings.json"),
            serde_json::to_string_pretty(&*map)?,
        )?;
        Ok(())
    }

    fn persist_timers(&self) -> SlcResult<()> {
        let map = self.timers.lock().unwrap();
        std::fs::write(self.slc_dir().join("timers.json"), serde_json::to_string_pretty(&*map)?)?;
        Ok(())
    }

    fn persist_records(&self) -> SlcResult<()> {
        let map = self.records.lock().unwrap();
        let flat: HashMap<String, Value> = map
            .iter()
            .map(|((c, k), v)| (format!("{c}\u{0}{k}"), v.clone()))
            .collect();
        std::fs::write(self.slc_dir().join("records.json"), serde_json::to_string_pretty(&flat)?)?;
        Ok(())
    }

    fn persist_seat(&self, seat: &Seat) -> SlcResult<()> {
        let path = self.root.join("seats").join(format!("{}.json", safe_file_name(&seat.seat_id)));
        std::fs::write(path, serde_json::to_string_pretty(seat)?)?;
        Ok(())
    }

    /// Write a document note: `{folder}/{safe_name}.md` (frontmatter + body).
    fn write_note(&self, doc: &Document) -> SlcResult<()> {
        let folder = doc.folder.clone().unwrap_or_else(|| doc.default_folder());
        let folder = sanitize_folder(&folder)?;
        let dir = self.root.join(&folder);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.md", safe_file_name(&doc.document_id)));
        let text = render_doc(doc)?;
        std::fs::write(path, text)?;
        self.index.lock().unwrap().insert(doc.document_id.clone(), IndexEntry::from_doc(doc));
        self.persist_index()?;
        Ok(())
    }

    /// Read + parse a note into a Document (files are the source of truth).
    fn read_note(&self, entry: &IndexEntry) -> SlcResult<Document> {
        let path = self.root.join(&sanitize_folder(&entry.folder)?)
            .join(format!("{}.md", safe_file_name(&entry.id)));
        let text = std::fs::read_to_string(&path)?;
        let (meta, body) = frontmatter_parse(&text);
        let meta = meta.unwrap_or_default();
        let metadata: crate::model::DocMeta =
            meta.get("metadata").cloned().map(|m| serde_json::from_value(m).unwrap_or_default()).unwrap_or_default();
        Ok(Document {
            document_id: meta.get("id").and_then(|v| v.as_str()).unwrap_or(&entry.id).to_string(),
            category: DocumentCategory::parse(
                meta.get("category").and_then(|v| v.as_str()).unwrap_or("custom"),
            )
            .unwrap_or(DocumentCategory::Custom),
            folder: Some(entry.folder.clone()),
            content: body,
            content_hash: meta
                .get("content_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            metadata,
            tags: meta.get("tags").and_then(|v| v.as_array()).map(|a| {
                a.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            }).unwrap_or_default(),
            auto_load: meta.get("auto_load").and_then(|v| v.as_array()).map(|a| {
                a.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            }).unwrap_or_default(),
            references: meta.get("references").and_then(|v| v.as_array()).map(|a| {
                a.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            }).unwrap_or_default(),
            seat_id: meta.get("seat_id").and_then(|v| v.as_str()).map(String::from),
            created_at: meta.get("created_at").and_then(|v| v.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now),
            updated_at: meta.get("updated_at").and_then(|v| v.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now),
            version: meta.get("version").and_then(|v| v.as_i64()).unwrap_or(entry.version),
            deleted_at: meta.get("deleted_at").and_then(|v| v.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)),
        })
    }

    /// Collect matching entries (KB or episodic by table) sorted + limited.
    fn select(
        &self,
        table: Table,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> Vec<IndexEntry> {
        let index = self.index.lock().unwrap();
        let mut out: Vec<&IndexEntry> = index
            .values()
            .filter(|e| {
                let is_kb = e.category != "history";
                if table == Table::Kb && !is_kb {
                    return false;
                }
                if table == Table::Episodic && is_kb {
                    return false;
                }
                e.matches(filter)
            })
            .collect();
        out.sort_by(|a, b| {
            let (af, bf) = match sort.field {
                super::SortField::CreatedAt => (&a.created_at, &b.created_at),
                super::SortField::UpdatedAt => (&a.updated_at, &b.updated_at),
            };
            let ord = af.cmp(bf);
            match sort.dir {
                SortDir::Asc => ord,
                SortDir::Desc => ord.reverse(),
            }
        });
        out.truncate(limit.min(out.len()));
        out.into_iter().cloned().collect()
    }

    async fn git_commit(&self) {
        if !self.auto_git_commit {
            return;
        }
        let root = self.root.clone();
        let author = self.git_author.clone();
        let git_lock = self.git_lock.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(_guard) = git_lock.lock() else { return }; // serialize git index.lock
            let author_name = author.split('<').next().unwrap_or("slc-mcp").trim().to_string();
            let run = |args: Vec<&str>| {
                std::process::Command::new("git")
                    .args(&args)
                    .current_dir(&root)
                    .env("GIT_AUTHOR_NAME", &author_name)
                    .env("GIT_AUTHOR_EMAIL", &author)
                    .output()
            };
            let failed = |out: &std::process::Output| {
                tracing::warn!(
                    "vault git: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            };
            match run(vec!["add", "-A"]) {
                Ok(out) if out.status.success() => {
                    let commit = run(vec!["commit", "-m", "slc: vault update", "--allow-empty"]);
                    match &commit {
                        Ok(c) if c.status.success() => {
                            // Push the vault to its upstream (best-effort) —
                            // configured remote only, silent otherwise.
                            if let Ok(p) = run(vec!["push", "-q"]) {
                                if !p.status.success() {
                                    tracing::debug!(
                                        "vault push: {}",
                                        String::from_utf8_lossy(&p.stderr).trim()
                                    );
                                }
                            }
                        }
                        Ok(c) => failed(c),
                        Err(e) => tracing::warn!("vault git commit error: {e}"),
                    }
                }
                Ok(out) => failed(&out),
                Err(e) => tracing::warn!("vault git add error: {e}"),
            }
        })
        .await;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    Kb,
    Episodic,
}

// ─────────────────────────────── frontmatter ───────────────────────────────

/// Parse YAML frontmatter: returns `(metadata, body)`.
pub fn frontmatter_parse(text: &str) -> (Option<Map<String, Value>>, String) {
    let Some(rest) = text.strip_prefix("---") else {
        return (None, text.trim().to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text.trim().to_string());
    };
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim().to_string();
    match serde_yaml::from_str::<Map<String, Value>>(yaml) {
        Ok(meta) => (Some(meta), body),
        Err(_) => (None, text.trim().to_string()),
    }
}

/// Render a document as frontmatter + body.
pub fn render_doc(doc: &Document) -> SlcResult<String> {
    let mut meta = Map::new();
    meta.insert("id".into(), json!(doc.document_id));
    meta.insert("category".into(), json!(doc.category.as_str()));
    meta.insert("content_hash".into(), json!(doc.content_hash));
    meta.insert("metadata".into(), serde_json::to_value(&doc.metadata)?);
    meta.insert("tags".into(), json!(doc.tags));
    meta.insert("auto_load".into(), json!(doc.auto_load));
    meta.insert("references".into(), json!(doc.references));
    meta.insert("seat_id".into(), json!(doc.seat_id));
    meta.insert("created_at".into(), json!(doc.created_at.to_rfc3339()));
    meta.insert("updated_at".into(), json!(doc.updated_at.to_rfc3339()));
    meta.insert("version".into(), json!(doc.version));
    meta.insert("deleted_at".into(), json!(doc.deleted_at.map(|d| d.to_rfc3339())));
    let yaml = serde_yaml::to_string(&meta).map_err(|e| SlcError::Parse(e.to_string()))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", doc.content))
}

// ─────────────────────────────── StorageBackend ───────────────────────────────

#[async_trait]
impl StorageBackend for ObsidianVaultStore {
    async fn kb_insert(&self, doc: &Document) -> SlcResult<()> {
        if !doc.category.is_kb() {
            return Err(SlcError::Storage("history docs go to the episodic store, not the KB".into()));
        }
        if self.index.lock().unwrap().contains_key(&doc.document_id) {
            return Err(SlcError::Storage(format!("document already exists: {}", doc.document_id)));
        }
        let doc = doc.clone();
        self.write_note(&doc)?;
        self.git_commit().await;
        Ok(())
    }

    async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        match entry {
            Some(e) if e.deleted_at.is_none() => Ok(Some(self.read_note(&e)?)),
            _ => Ok(None),
        }
    }

    async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        let Some(mut doc) = entry.map(|e| self.read_note(&e)).transpose()? else {
            return Ok(false);
        };
        doc.content = content.to_string();
        doc.content_hash = content_hash(content);
        doc.version += 1;
        doc.updated_at = Utc::now();
        self.write_note(&doc)?;
        self.git_commit().await;
        Ok(true)
    }

    async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool> {
        let exists = self.index.lock().unwrap().contains_key(&doc.document_id);
        if exists {
            self.kb_update_content(&doc.document_id, &doc.content).await?;
            return Ok(true);
        }
        self.kb_insert(doc).await?;
        Ok(false)
    }

    async fn kb_replace(&self, doc: &Document) -> SlcResult<bool> {
        if !doc.category.is_kb() {
            return Err(SlcError::Storage("history docs go to the episodic store, not the KB".into()));
        }
        let exists = self.index.lock().unwrap().contains_key(&doc.document_id);
        let doc = doc.clone();
        self.write_note(&doc)?;
        self.git_commit().await;
        Ok(exists)
    }

    async fn kb_find(&self, filter: &DocFilter, sort: &DocSort, limit: usize) -> SlcResult<Vec<Document>> {
        let entries = self.select(Table::Kb, filter, sort, limit);
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            out.push(self.read_note(&e)?);
        }
        Ok(out)
    }

    async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        Ok(self.select(Table::Kb, filter, &DocSort::default(), usize::MAX).len() as u64)
    }

    async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        let entries = self.select(Table::Kb, filter, &DocSort::default(), usize::MAX);
        let mut changed = 0;
        for e in entries {
            let mut doc = self.read_note(&e)?;
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
            doc.metadata = m;
            doc.updated_at = Utc::now();
            self.write_note(&doc)?;
            changed += 1;
        }
        self.git_commit().await;
        Ok(changed)
    }

    async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        let Some(mut doc) = entry.map(|e| self.read_note(&e)).transpose()? else {
            return Ok(false);
        };
        if doc.deleted_at.is_some() {
            return Ok(false);
        }
        doc.deleted_at = Some(Utc::now());
        doc.version += 1;
        self.write_note(&doc)?;
        self.git_commit().await;
        Ok(true)
    }

    async fn kb_restore(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        let Some(mut doc) = entry.map(|e| self.read_note(&e)).transpose()? else {
            return Ok(false);
        };
        if doc.deleted_at.is_none() {
            return Ok(false);
        }
        doc.deleted_at = None;
        doc.version += 1;
        self.write_note(&doc)?;
        self.git_commit().await;
        Ok(true)
    }

    async fn kb_purge(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().remove(document_id);
        let Some(e) = entry else { return Ok(false) };
        let path = self.root.join(&sanitize_folder(&e.folder)?)
            .join(format!("{}.md", safe_file_name(&e.id)));
        let _ = tokio::fs::remove_file(path).await;
        self.delete_embeddings(document_id).await?;
        self.persist_index()?;
        self.git_commit().await;
        Ok(true)
    }

    async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>> {
        let cutoff = days.map(|d| Utc::now() - chrono::Duration::days(d));
        let entries = self.select(Table::Kb, &DocFilter { deleted: true, ..Default::default() }, &DocSort::default(), usize::MAX);
        let mut out = Vec::new();
        for e in entries {
            let doc = self.read_note(&e)?;
            if let Some(deleted) = doc.deleted_at {
                if cutoff.map_or(true, |c| deleted <= c) {
                    out.push(doc);
                }
            }
        }
        Ok(out)
    }

    async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64> {
        let graveyard = self.kb_graveyard(Some(days)).await?;
        let mut n = 0;
        for doc in graveyard {
            if self.kb_purge(&doc.document_id).await? {
                n += 1;
            }
        }
        Ok(n)
    }

    // ── episodic ────────────────────────────────────────────────

    async fn episodic_insert(&self, doc: &Document) -> SlcResult<()> {
        if doc.category != DocumentCategory::History {
            return Err(SlcError::Storage("only history docs go to the episodic store".into()));
        }
        // Seat-less episodic docs are allowed (legacy import): they are not
        // picked up by any per-seat pipeline, just stored as diary.
        if self.index.lock().unwrap().contains_key(&doc.document_id) {
            return Err(SlcError::Storage(format!("document already exists: {}", doc.document_id)));
        }
        let doc = doc.clone();
        self.write_note(&doc)?;
        self.git_commit().await;
        Ok(())
    }

    async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool> {
        if self.index.lock().unwrap().contains_key(&doc.document_id) {
            self.kb_update_content(&doc.document_id, &doc.content).await?;
            return Ok(true);
        }
        self.episodic_insert(doc).await?;
        Ok(false)
    }

    async fn episodic_find(&self, filter: &DocFilter, sort: &DocSort, limit: usize) -> SlcResult<Vec<Document>> {
        let entries = self.select(Table::Episodic, filter, sort, limit);
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            out.push(self.read_note(&e)?);
        }
        Ok(out)
    }

    async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        Ok(self.select(Table::Episodic, filter, &DocSort::default(), usize::MAX).len() as u64)
    }

    async fn episodic_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        let entries = self.select(Table::Episodic, filter, &DocSort::default(), usize::MAX);
        let mut changed = 0;
        for e in entries {
            let mut doc = self.read_note(&e)?;
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
            doc.metadata = m;
            doc.updated_at = Utc::now();
            self.write_note(&doc)?;
            changed += 1;
        }
        self.git_commit().await;
        Ok(changed)
    }

    async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().remove(document_id);
        let Some(e) = entry else { return Ok(false) };
        let path = self.root.join(&sanitize_folder(&e.folder)?)
            .join(format!("{}.md", safe_file_name(&e.id)));
        let _ = tokio::fs::remove_file(path).await;
        self.persist_index()?;
        self.git_commit().await;
        Ok(true)
    }

    // ── embeddings ──────────────────────────────────────────────

    async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()> {
        let mut map = self.embeddings.lock().unwrap();
        for r in records {
            map.entry(r.document_id.clone()).or_default().push(r.clone());
        }
        drop(map);
        self.persist_embeddings()
    }

    async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>> {
        let map = self.embeddings.lock().unwrap();
        Ok(map.get(document_id).and_then(|chunks| chunks.first()).map(|c| c.embedding.clone()))
    }

    async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>> {
        let map = self.embeddings.lock().unwrap();
        Ok(map.get(document_id).cloned().unwrap_or_default())
    }

    async fn all_embeddings(&self, scope: EmbeddingScope, seat_id: Option<&str>) -> SlcResult<Vec<EmbeddingRecord>> {
        let map = self.embeddings.lock().unwrap();
        let mut out = Vec::new();
        for chunks in map.values() {
            for c in chunks {
                if c.scope != scope {
                    continue;
                }
                if let Some(seat) = seat_id {
                    if c.seat_id.as_deref() != Some(seat) {
                        continue;
                    }
                }
                out.push(c.clone());
            }
        }
        Ok(out)
    }

    async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()> {
        self.embeddings.lock().unwrap().remove(document_id);
        self.persist_embeddings()
    }

    // ── seats ───────────────────────────────────────────────────

    async fn insert_seat(&self, seat: &Seat) -> SlcResult<()> {
        self.seats.lock().unwrap().insert(seat.seat_id.clone(), seat.clone());
        self.persist_seat(seat)
    }

    async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
        Ok(self.seats.lock().unwrap().get(seat_id).cloned())
    }

    async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>> {
        let mut seats: Vec<Seat> = self
            .seats
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.status == SeatStatus::Active)
            .cloned()
            .collect();
        seats.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));
        seats.truncate(limit);
        Ok(seats)
    }

    async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool> {
        let mut seats = self.seats.lock().unwrap();
        let Some(seat) = seats.get_mut(seat_id) else { return Ok(false) };
        seat.last_accessed = Utc::now();
        let seat = seat.clone();
        drop(seats);
        self.persist_seat(&seat)?;
        Ok(true)
    }

    async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool> {
        let mut seats = self.seats.lock().unwrap();
        let Some(seat) = seats.get_mut(seat_id) else { return Ok(false) };
        seat.status = status;
        let seat = seat.clone();
        drop(seats);
        self.persist_seat(&seat)?;
        Ok(true)
    }

    async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let mut seats = self.seats.lock().unwrap();
        let Some(seat) = seats.get_mut(seat_id) else { return Ok(false) };
        seat.active_task_id = Some(task_id.into());
        // Task activation is document activation too (unified anchor).
        seat.active_document_id = Some(task_id.into());
        let seat = seat.clone();
        drop(seats);
        self.persist_seat(&seat)?;
        Ok(true)
    }

    async fn set_seat_active_document(&self, seat_id: &str, document_id: Option<&str>) -> SlcResult<bool> {
        let mut seats = self.seats.lock().unwrap();
        let Some(seat) = seats.get_mut(seat_id) else { return Ok(false) };
        seat.active_document_id = document_id.map(String::from);
        let seat = seat.clone();
        drop(seats);
        self.persist_seat(&seat)?;
        Ok(true)
    }

    async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
        let seats = self.seats.lock().unwrap();
        let Some(seat) = seats.get(seat_id) else { return Ok(None) };
        // Unified field first; fall back to the legacy task pointer.
        Ok(seat.active_document_id.clone().or_else(|| seat.active_task_id.clone()))
    }

    async fn incr_seat_stats(&self, seat_id: &str, tool_name: &str, tokens_used: i64) -> SlcResult<bool> {
        let mut seats = self.seats.lock().unwrap();
        let Some(seat) = seats.get_mut(seat_id) else { return Ok(false) };
        seat.usage_stats.total_requests += 1;
        seat.usage_stats.total_tokens += tokens_used;
        let count = seat.usage_stats.tools_used.get(tool_name).and_then(|v| v.as_i64()).unwrap_or(0) + 1;
        seat.usage_stats.tools_used.insert(tool_name.into(), json!(count));
        seat.last_accessed = Utc::now();
        let seat = seat.clone();
        drop(seats);
        self.persist_seat(&seat)?;
        Ok(true)
    }

    // ── timers ──────────────────────────────────────────────────

    async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
        self.timers.lock().unwrap().insert(timer.timer_id.clone(), timer.clone());
        self.persist_timers()
    }

    async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>> {
        Ok(self.timers.lock().unwrap().get(timer_id).cloned())
    }

    async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
        let timers = self.timers.lock().unwrap();
        let mut out: Vec<PersistedTimer> = timers
            .values()
            .filter(|t| t.is_active && seat_id.map_or(true, |s| t.seat_id == s))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.next_fire_at.cmp(&b.next_fire_at));
        Ok(out)
    }

    async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()> {
        let mut timers = self.timers.lock().unwrap();
        if let Some(t) = timers.get_mut(timer_id) {
            t.last_fired_at = Some(now);
        }
        drop(timers);
        self.persist_timers()
    }

    // ── records ─────────────────────────────────────────────────

    async fn put_record(&self, collection: &str, key: &str, value: &Value) -> SlcResult<()> {
        self.records.lock().unwrap().insert((collection.to_string(), key.to_string()), value.clone());
        self.persist_records()
    }

    async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<Value>> {
        Ok(self.records.lock().unwrap().get(&(collection.to_string(), key.to_string())).cloned())
    }

    async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool> {
        let existed = self.records.lock().unwrap().remove(&(collection.to_string(), key.to_string())).is_some();
        if existed {
            self.persist_records()?;
        }
        Ok(existed)
    }

    async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, Value)>> {
        let records = self.records.lock().unwrap();
        let mut out: Vec<(String, Value)> = records
            .iter()
            .filter(|((c, _), _)| c == collection)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn health_check(&self) -> bool {
        self.root.exists()
    }

    async fn close(&self) -> SlcResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use crate::model::{DocLevel, DocMeta};

    fn tmp_vault(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("slc-test-{tag}-{}", uuid::Uuid::new_v4().simple()))
    }

    #[tokio::test]
    async fn vault_layout_and_names() {
        let root = tmp_vault("layout");
        let store = ObsidianVaultStore::open(&root, false).unwrap();

        // KB doc with explicit human folder + name.
        let mut m = DocMeta::default();
        m.doc_type = Some("plan".into());
        let doc = Document::with_folder(
            "vassista-phase-1",
            DocumentCategory::Project,
            Some("projects/vassista".into()),
            "# Phase 1 plan\n\n- audio half",
            m,
            vec!["priority:high".into()],
            None,
        );
        store.kb_insert(&doc).await.unwrap();

        let path = root.join("projects/vassista/vassista-phase-1.md");
        assert!(path.exists(), "expected note at {path:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("id: vassista-phase-1"));
        assert!(text.contains("# Phase 1 plan"));

        // Roundtrip via fresh open (hand-edit simulation: index rebuilt).
        let store2 = ObsidianVaultStore::open(&root, false).unwrap();
        let loaded = store2.kb_get("vassista-phase-1").await.unwrap().unwrap();
        assert_eq!(loaded.content, "# Phase 1 plan\n\n- audio half");
        assert_eq!(loaded.metadata.doc_type.as_deref(), Some("plan"));

        // History goes to history/YYYY/MM/ and is invisible to KB search.
        let mut hm = DocMeta::default();
        hm.doc_level = Some(DocLevel::L1);
        hm.seat_id = Some("seat_1".into());
        let evt = Document::new(
            "session-2026-08-13-morning",
            DocumentCategory::History,
            "worked on the STT pipeline",
            hm,
            vec![],
            Some("seat_1".into()),
        );
        store2.episodic_insert(&evt).await.unwrap();
        let rel = root.join("history").join(format!("{:04}", Utc::now().year())).join(format!("{:02}", Utc::now().month()));
        assert!(rel.join("session-2026-08-13-morning.md").exists());

        let kb_count = store2.kb_count(&DocFilter::default()).await.unwrap();
        assert_eq!(kb_count, 1, "history must not be in KB");
        let ep_count = store2.episodic_count(&DocFilter::default()).await.unwrap();
        assert_eq!(ep_count, 1);
        assert!(store2.kb_insert(&evt).await.is_err(), "history rejected by kb_insert");
    }

    #[tokio::test]
    async fn episodic_compression_queries() {
        let root = tmp_vault("compress");
        let store = ObsidianVaultStore::open(&root, false).unwrap();
        let seat = "seat_c";

        for i in 0..5 {
            let mut m = DocMeta::default();
            m.doc_level = Some(DocLevel::L1);
            m.seat_id = Some(seat.into());
            store
                .episodic_insert(&Document::new(
                    format!("event-{i}"),
                    DocumentCategory::History,
                    format!("event {i}"),
                    m,
                    vec![],
                    Some(seat.into()),
                ))
                .await
                .unwrap();
        }

        let f = DocFilter {
            seat_id: Some(seat.into()),
            doc_level: Some(DocLevel::L1),
            has_compression_batch: Some(false),
            ..Default::default()
        };
        assert_eq!(store.episodic_count(&f).await.unwrap(), 5);

        let patch = MetaPatch {
            set_archived: Some(true),
            set_compression_batch_id: Some("batch_1".into()),
            ..Default::default()
        };
        assert_eq!(store.episodic_patch_meta(&f, &patch).await.unwrap(), 5);
        assert_eq!(
            store
                .episodic_count(&DocFilter { has_compression_batch: Some(false), ..Default::default() })
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn embeddings_sidecar() {
        let root = tmp_vault("emb");
        let store = ObsidianVaultStore::open(&root, false).unwrap();
        store
            .insert_embeddings(&[EmbeddingRecord {
                document_id: "doc-a".into(),
                chunk_index: 0,
                chunk_total: 1,
                embedding: vec![0.5, 0.25],
                embedding_model: "bge-m3".into(),
                embedding_dimension: 2,
                generated_at: Utc::now(),
                scope: EmbeddingScope::Public,
                seat_id: None,
            }])
            .await
            .unwrap();
        assert_eq!(store.get_embedding("doc-a").await.unwrap(), Some(vec![0.5, 0.25]));
    }
    #[test]
    fn sanitize_folder_accepts_nested_relative_and_rejects_escapes() {
        // Valid Obsidian layouts.
        assert_eq!(sanitize_folder("projects/vassista").unwrap(), "projects/vassista");
        assert_eq!(sanitize_folder("history/2026/08").unwrap(), "history/2026/08");
        // Escapes must be rejected (path traversal / absolute / windows).
        for bad in [
            "../../tmp/x",
            "projects/../..",
            "/etc",
            "a/b/../../c",
            "..",
            "a\\..\\b",
            "a:b",
            "",
            "  ",
        ] {
            assert!(sanitize_folder(bad).is_err(), "must reject {bad:?}");
        }
    }

}
