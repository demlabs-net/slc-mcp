//! Migration from the Python SLC legacy storage into the Rust engine.
//!
//! The legacy Obsidian vault layout (see `slc-mcp.legacy`):
//!
//! ```text
//! vault/<collection>/<stem>.md      — документ = markdown + YAML frontmatter
//! vault/embeddings/…                — JSON-файлы (перегенерируются, не мигрируем)
//! vault/.slc-index/…                — индексы (пересоздаются)
//! ```
//!
//! Collections: `focuses`, `ideas` (убраны из новой модели — пропускаются),
//! `knowledge_base`, `seats`, `history` / episodic, `tasks`, `projects`.
//!
//! Migration rules (user-approved):
//!
//! - **id переделываются в нормальные человекочитаемые имена**: из
//!   frontmatter `name`/`title` или первой строки тела строится slug
//!   с префиксом категории (`project_…`, `task_…`, `skill_…`, `doc_…`);
//!   пустые/мусорные имена — детерминированный fallback на старый id.
//! - **в Obsidian vault цель** — документы раскладываются по папкам
//!   категорий (`projects/`, `tasks/`, `skills/`, `knowledge/`, `history/…`,
//!   `focuses/`, `seats/`), как делает новый `ObsidianVaultStore`.
//! - **embeddings и идеи не переносятся** (перегенерация; концепция идей
//!   удалена).

use crate::error::{SlcError, SlcResult};
use crate::llm::LlmClient;
use crate::model::{content_hash, DocMeta, Document, DocumentCategory, Seat, SeatStatus, UsageStats};
use crate::storage::StorageBackend;
use bson::{doc, Bson, Document as BsonDoc};
use chrono::{DateTime, Utc};
use mongodb::{Client, Collection};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

/// Result of a migration run.
#[derive(Debug, Default, serde::Serialize)]
pub struct MigrateReport {
    pub documents: usize,
    pub focuses: usize,
    pub seats: usize,
    pub skipped_ideas: usize,
    pub skipped_embeddings: usize,
    pub errors: Vec<String>,
}

/// Parse a legacy markdown file: YAML frontmatter (between `---` lines) +
/// body.
fn parse_legacy_md(raw: &str) -> (Map<String, Value>, String) {
    let trimmed = raw.trim_start_matches('\u{feff}');
    let body = if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let body = rest[end + 4..].trim_start_matches('\n').to_string();
            let parsed: Map<String, Value> = serde_yaml::from_str(fm).unwrap_or_default();
            return (parsed, body);
        }
        (Map::new(), trimmed.to_string())
    } else {
        (Map::new(), trimmed.to_string())
    };
    body
}

/// Human-readable id from a document's name/title/body.
fn human_id(prefix: &str, fm: &Map<String, Value>, body: &str) -> String {
    let raw = fm
        .get("name")
        .or_else(|| fm.get("title"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            body.lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim_matches('#').trim().to_string())
        })
        .unwrap_or_default();
    let slug: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let slug = slug.chars().take(48).collect::<String>();
    if slug.is_empty() {
        // Fallback: deterministic name from the old id.
        format!(
            "{prefix}legacy_{}",
            fm.get("document_id")
                .or_else(|| fm.get("focus_id"))
                .or_else(|| fm.get("task_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("doc")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(24)
                .collect::<String>()
        )
    } else {
        format!("{prefix}{slug}")
    }
}

/// Category id prefix. Only the diary keeps a prefix (`history_…`); all
/// other documents are named without one — the folder tree (`docs/`,
/// `docs/projects/<p>/`, `tasks/`) already carries the category context.
fn category_prefix(cat: DocumentCategory) -> &'static str {
    if cat == DocumentCategory::History {
        "history_"
    } else {
        ""
    }
}

/// Migrate a legacy Obsidian vault into `target` (any storage backend).
pub async fn migrate_legacy_vault(
    src: &Path,
    target: &dyn StorageBackend,
) -> SlcResult<MigrateReport> {
    let mut report = MigrateReport::default();

    // ── knowledge_base → KB documents (RAG) ──
    if let Ok(entries) = std::fs::read_dir(src.join("knowledge_base")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(e) => {
                    report.errors.push(format!("read {}: {e}", path.display()));
                    continue;
                }
            };
            let (fm, body) = parse_legacy_md(&raw);
            let cat = fm
                .get("category")
                .and_then(|v| v.as_str())
                .and_then(DocumentCategory::parse)
                .unwrap_or(DocumentCategory::Documentation);
            let old_id = fm
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("legacy_doc")
                .to_string();
            let new_id = human_id(category_prefix(cat), &fm, &body);
            let seat = fm.get("seat_id").and_then(|v| v.as_str()).map(str::to_string);
            let mut meta = crate::model::DocMeta::default();
            if let Some(t) = fm.get("doc_type").and_then(|v| v.as_str()) {
                meta.doc_type = Some(t.to_string());
            }
            let tags: Vec<String> = fm
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let mut doc = Document::new(new_id.clone(), cat, body, meta, tags, seat);
            doc.metadata.extra.insert(
                "legacy_id".into(),
                json!(old_id),
            );
            if let Err(e) = target.kb_upsert(&doc).await {
                report.errors.push(format!("kb upsert {new_id}: {e}"));
            } else {
                report.documents += 1;
            }
        }
    }

    // ── history / episodic ──
    for dir in ["history", "episodic"] {
        if let Ok(entries) = std::fs::read_dir(src.join(dir)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(&path) else { continue };
                let (fm, body) = parse_legacy_md(&raw);
                let old_id = fm
                    .get("document_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("legacy_ev")
                    .to_string();
                let new_id = human_id("history_", &fm, &body);
                let seat = fm.get("seat_id").and_then(|v| v.as_str()).map(str::to_string);
                let doc = Document::new(
                    new_id.clone(),
                    DocumentCategory::History,
                    body,
                    Default::default(),
                    vec![],
                    seat,
                );
                if let Err(e) = target.episodic_upsert(&doc).await {
                    report.errors.push(format!("episodic {new_id} ({old_id}): {e}"));
                } else {
                    report.documents += 1;
                }
            }
        }
    }

    // ── focuses → `focuses` records ──
    if let Ok(entries) = std::fs::read_dir(src.join("focuses")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let (fm, body) = parse_legacy_md(&raw);
            let seat = fm.get("seat_id").and_then(|v| v.as_str()).unwrap_or("legacy");
            let focus_id = fm
                .get("focus_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&human_id("focus_", &fm, &body))
                .to_string();
            let item = json!({
                "focus_id": focus_id,
                "seat_id": seat,
                "mind_type": "shared",
                "title": fm.get("name").or_else(|| fm.get("title")).and_then(|v| v.as_str()).unwrap_or(&body).to_string(),
                "description": body,
                "priority": fm.get("priority").and_then(|v| v.as_i64()).unwrap_or(1),
                "depends_on": [],
                "reminder_count": 0,
                "created_at": fm.get("created_at").and_then(|v| v.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now),
                "archived": false,
            });
            if let Err(e) = target.put_record("focuses", &focus_id, &item).await {
                report.errors.push(format!("focus {focus_id}: {e}"));
            } else {
                report.focuses += 1;
            }
        }
    }

    // ── seats ──
    if let Ok(entries) = std::fs::read_dir(src.join("seats")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let (fm, _body) = parse_legacy_md(&raw);
            let seat_id = fm
                .get("seat_id")
                .and_then(|v| v.as_str())
                .unwrap_or("legacy_seat")
                .to_string();
            let now = Utc::now();
            let seat = Seat {
                seat_id,
                name: fm.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string(),
                status: SeatStatus::Active,
                created_at: now,
                last_accessed: now,
                expires_at: None,
                metadata: fm
                    .get("metadata")
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default(),
                active_task_id: None,
                active_document_id: None,
                context: Map::new(),
                usage_stats: UsageStats::default(),
            };
            if let Err(e) = target.insert_seat(&seat).await {
                report.errors.push(format!("seat {}: {e}", seat.seat_id));
            } else {
                report.seats += 1;
            }
        }
    }

    // ── ideas / embeddings — skipped by design ──
    if src.join("ideas").is_dir() {
        report.skipped_ideas = std::fs::read_dir(src.join("ideas"))
            .map(|it| it.flatten().filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md")).count())
            .unwrap_or(0);
    }
    if src.join("embeddings").is_dir() {
        report.skipped_embeddings = std::fs::read_dir(src.join("embeddings"))
            .map(|it| it.flatten().count())
            .unwrap_or(0);
    }

    Ok(report)
}

// ─────────────────────────── MongoDB legacy migration ──────────────────────────

/// Options for `migrate_legacy_mongo` (`slc-mcp migrate --from-mongo`).
#[derive(Debug, Clone)]
pub struct MongoMigrateOptions {
    /// Legacy MongoDB URI (e.g. `mongodb://127.0.0.1:27017`).
    pub uri: String,
    /// Legacy database name (the Python server used `slc_mcp`).
    pub database: String,
}

/// Result of a Mongo migration run.
#[derive(Debug, Default, serde::Serialize)]
pub struct MongoMigrateReport {
    /// KB documents imported (RAG-eligible categories).
    pub documents: usize,
    /// Episodic history docs imported (diary, not RAG).
    pub history: usize,
    /// Project documents imported (`docs/projects/<slug>/<slug>.md`).
    pub projects: usize,
    /// Task documents imported (`docs/projects/<p>/tasks/` or `tasks/`).
    pub tasks: usize,
    /// Seats imported.
    pub seats: usize,
    /// KB ids renamed by the LLM (`--rename-with-ai`).
    pub renamed_with_ai: usize,
    /// Soft-deleted docs skipped.
    pub skipped_deleted: usize,
    pub errors: Vec<String>,
}

/// Normalized legacy document (from `slc_mcp.knowledge_base`).
struct LegacyDoc {
    document_id: String,
    category: Option<String>,
    content: String,
    doc_type: Option<String>,
    metadata: Map<String, Value>,
    tags: Vec<String>,
    auto_load: Vec<String>,
    references: Vec<String>,
    seat_id: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    version: i64,
    deleted_at: Option<DateTime<Utc>>,
}

/// bson → serde_json (dates as RFC 3339 strings; exotic variants → debug).
fn bson_to_json(v: &Bson) -> Value {
    match v {
        Bson::Double(f) => json!(f),
        Bson::String(s) => json!(s),
        Bson::Array(a) => Value::Array(a.iter().map(bson_to_json).collect()),
        Bson::Document(d) => Value::Object(
            d.iter().map(|(k, x)| (k.clone(), bson_to_json(x))).collect(),
        ),
        Bson::Boolean(b) => json!(b),
        Bson::Int32(i) => json!(i),
        Bson::Int64(i) => json!(i),
        Bson::DateTime(dt) => json!(dt.to_chrono().to_rfc3339()),
        Bson::Null | Bson::Undefined | Bson::MinKey | Bson::MaxKey => Value::Null,
        other => json!(format!("{other:?}")),
    }
}

fn bson_doc_to_map(d: &BsonDoc) -> Map<String, Value> {
    match bson_to_json(&Bson::Document(d.clone())) {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn bson_i64(v: &Bson) -> Option<i64> {
    match v {
        Bson::Int32(i) => Some(*i as i64),
        Bson::Int64(i) => Some(*i),
        _ => None,
    }
}

fn bson_strs(d: &BsonDoc, key: &str) -> Vec<String> {
    d.get_array(key)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn extract_legacy_doc(raw: &BsonDoc) -> Option<LegacyDoc> {
    let document_id = raw.get_str("document_id").ok()?.to_string();
    let category = raw.get_str("category").ok().map(String::from);
    let content = match raw.get("content") {
        Some(Bson::String(s)) => s.clone(),
        Some(other) => serde_json::to_string_pretty(&bson_to_json(other))
            .unwrap_or_else(|_| other.to_string()),
        None => String::new(),
    };
    let mut metadata = raw
        .get_document("metadata")
        .map(bson_doc_to_map)
        .unwrap_or_default();
    let doc_type = metadata
        .remove("doc_type")
        .or_else(|| metadata.remove("type"))
        .and_then(|v| v.as_str().map(String::from));
    Some(LegacyDoc {
        document_id,
        category,
        content,
        doc_type,
        metadata,
        tags: bson_strs(raw, "tags"),
        auto_load: bson_strs(raw, "auto_load"),
        references: bson_strs(raw, "references"),
        seat_id: raw.get_str("seat_id").ok().map(String::from),
        created_at: raw.get_datetime("created_at").ok().map(|d| d.to_chrono()),
        updated_at: raw.get_datetime("updated_at").ok().map(|d| d.to_chrono()),
        version: raw.get("version").and_then(bson_i64).unwrap_or(1),
        deleted_at: raw.get_datetime("deleted_at").ok().map(|d| d.to_chrono()),
    })
}

fn doc_category(doc: &LegacyDoc) -> DocumentCategory {
    doc.category
        .as_deref()
        .and_then(DocumentCategory::parse)
        .unwrap_or(DocumentCategory::Custom)
}

/// Lowercase ascii slug (words joined with `_`, ≤48 chars, empty → `""`).
fn slugify(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_")
        .chars()
        .take(48)
        .collect()
}

/// Deterministic readable id for a legacy diary entry: date + id tail.
/// History docs are not renamed by the LLM — there are hundreds of them.
fn history_id(old_id: &str, created: Option<DateTime<Utc>>) -> String {
    let date = created
        .map(|d| d.format("%Y_%m_%d").to_string())
        .unwrap_or_else(|| "undated".into());
    let stripped = old_id.strip_prefix("history_").unwrap_or(old_id);
    let tail: String = stripped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let tail: String = tail.chars().rev().take(12).collect();
    format!("history_{date}_{}", tail.chars().rev().collect::<String>())
}

/// Human slug from the first heading / first non-empty line.
fn doc_slug(doc: &LegacyDoc) -> String {
    let raw = doc
        .content
        .lines()
        .find(|l| {
            let t = l.trim();
            !t.is_empty()
        })
        .map(|l| l.trim().trim_start_matches('#').trim().to_string())
        .unwrap_or_default();
    slugify(&raw)
}

/// Insert into `used`, appending `_2`, `_3`, … on collision.
fn unique(base: String, used: &mut HashSet<String>) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}_{n}");
        if used.insert(cand.clone()) {
            return cand;
        }
        n += 1;
    }
}

/// KB id: AI slug (if provided) or deterministic fallback (heading slug →
/// sanitized legacy id), deduped against `used`. No category prefixes —
/// the folder tree carries the category context.
fn new_kb_id(doc: &LegacyDoc, ai: Option<String>, used: &mut HashSet<String>) -> String {
    let base = match ai {
        Some(slug) if !slug.is_empty() => {
            format!("{}{}", category_prefix(doc_category(doc)), slug)
        }
        _ => {
            let slug = doc_slug(doc);
            if slug.is_empty() {
                // Deterministic fallback on the legacy id itself (readable
                // and stable: `core_slc_manifest`, `documentation_abc…`).
                doc.document_id
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .take(64)
                    .collect::<String>()
            } else {
                format!("{}{}", category_prefix(doc_category(doc)), slug)
            }
        }
    };
    unique(base, used)
}

/// Ask the LLM (reasoning model from the environment) for a short meaningful
/// id for the document; sanitized to an ascii slug (empty on failure).
async fn ai_slug(llm: &dyn LlmClient, doc: &LegacyDoc) -> Option<String> {
    let snippet: String = doc.content.chars().take(600).collect();
    let prompt = format!(
        "Ты — сервис нейминга документов памяти SLC. По содержимому документа придумай \
         короткий осмысленный id (slug): строчные ascii-буквы и цифры, слова через \
         подчёркивание, максимум 6 слов, без префикса категории. Если текущий id уже \
         осмысленный — можешь оставить его. Верни ТОЛЬКО id, без пояснений.\n\n\
         Текущий id: {}\nДокумент:\n{}",
        doc.document_id, snippet
    );
    let out = llm.reason(&prompt).await.ok()?;
    let slug = slugify(&out);
    if slug.is_empty() { None } else { Some(slug) }
}

/// Rewrite auto_load/references through the old→new id map (unknown ids —
/// skipped docs, external refs — are kept as-is).
fn fix_links(ids: &[String], id_map: &HashMap<String, String>) -> Vec<String> {
    ids.iter()
        .map(|id| id_map.get(id).cloned().unwrap_or_else(|| id.clone()))
        .collect()
}

/// Legacy seat → new `Seat`. All seats are imported ACTIVE and non-expiring
/// (every legacy seat id keeps working); the original status/expiry/active
/// task are preserved in `metadata.legacy_*`. Handles both field layouts:
/// the standard `last_accessed`/`usage_stats` and the recovered-from-JSON
/// `last_activity`/`statistics`.
fn extract_legacy_seat(raw: &BsonDoc) -> Option<Seat> {
    let seat_id = raw.get_str("seat_id").ok()?.to_string();
    let now = Utc::now();
    let created_at = raw
        .get_datetime("created_at")
        .ok()
        .map(|d| d.to_chrono())
        .unwrap_or(now);
    let last_accessed = raw
        .get_datetime("last_accessed")
        .ok()
        .map(|d| d.to_chrono())
        .or_else(|| raw.get_datetime("last_activity").ok().map(|d| d.to_chrono()))
        .unwrap_or(created_at);

    let mut metadata = raw
        .get_document("metadata")
        .map(bson_doc_to_map)
        .unwrap_or_default();
    if let Ok(st) = raw.get_str("status") {
        if st != "active" {
            metadata.insert("legacy_status".into(), json!(st));
        }
    }
    if let Ok(exp) = raw.get_datetime("expires_at") {
        metadata.insert("legacy_expires_at".into(), json!(exp.to_chrono().to_rfc3339()));
    }
    if let Ok(task) = raw.get_str("active_task_id") {
        metadata.insert("legacy_active_task_id".into(), json!(task));
    }

    // Usage stats: standard `usage_stats` layout or recovered `statistics`.
    let mut total_requests = 0i64;
    let mut total_tokens = 0i64;
    let mut tools_used = Map::new();
    for key in ["usage_stats", "statistics"] {
        let Ok(us) = raw.get_document(key) else { continue };
        for (k, v) in us.iter() {
            match k.as_str() {
                "total_requests" => {
                    if let Some(i) = bson_i64(v) {
                        total_requests = i;
                    }
                }
                "total_tokens" => {
                    if let Some(i) = bson_i64(v) {
                        total_tokens = i;
                    }
                }
                "tools_used" | "tool_usage" => {
                    if let Bson::Document(d) = v {
                        tools_used = bson_doc_to_map(d);
                    }
                }
                _ => {}
            }
        }
    }

    Some(Seat {
        seat_id,
        name: raw.get_str("name").unwrap_or("unnamed").to_string(),
        status: SeatStatus::Active,
        created_at,
        last_accessed,
        expires_at: None,
        metadata,
        active_task_id: None,
        active_document_id: None,
        context: raw
            .get_document("context")
            .map(bson_doc_to_map)
            .unwrap_or_default(),
        usage_stats: UsageStats {
            total_requests,
            total_tokens,
            tools_used,
        },
    })
}

/// Migrate the legacy Python MongoDB (`knowledge_base`, `projects`, `tasks`
/// and `seats` collections) into `target` (any new backend; Obsidian vault
/// by default).
///
/// - KB documents keep their ids by default; with `rename_with_ai` the
///   reasoning LLM from the environment proposes meaningful ids and
///   `auto_load`/`references` are rewritten to the new ids.
/// - `history` docs go to the episodic store (diary layout), not the KB.
/// - Projects become `Project` documents (`docs/projects/<slug>/`), tasks
///   are bound to their legacy project via `metadata.extra["project"]` and
///   land in `docs/projects/<p>/tasks/` (or `tasks/` without a project).
/// - Seats are imported Active and non-expiring (all seat ids keep working);
///   the original status/expiry survive in `metadata.legacy_*`.
pub async fn migrate_legacy_mongo(
    opts: &MongoMigrateOptions,
    target: &dyn StorageBackend,
    llm: Option<&dyn LlmClient>,
    rename_with_ai: bool,
) -> SlcResult<MongoMigrateReport> {
    let client = Client::with_uri_str(&opts.uri)
        .await
        .map_err(|e| SlcError::Storage(format!("mongodb connect: {e}")))?;
    let db = client.database(&opts.database);
    let mut report = MongoMigrateReport::default();
    let now = Utc::now();

    // ── projects → Project documents ──
    let mut project_by_id: HashMap<String, String> = HashMap::new(); // legacy project_id → new id
    let mut jobs: Vec<(Document, bool)> = Vec::new(); // (document, is_history)
    let mut used = HashSet::new();
    {
        let projects: Collection<BsonDoc> = db.collection("projects");
        let mut cursor = projects
            .find(doc! {})
            .await
            .map_err(|e| SlcError::Storage(format!("projects find: {e}")))?;
        while cursor
            .advance()
            .await
            .map_err(|e| SlcError::Storage(format!("cursor: {e}")))?
        {
            let raw: BsonDoc = cursor
                .deserialize_current()
                .map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            let Some(project_id) = raw.get_str("project_id").ok().map(String::from) else {
                continue;
            };
            let name = raw.get_str("name").unwrap_or("").to_string();
            let slug = slugify(&name);
            let new_id = unique(
                if slug.is_empty() {
                    project_id
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .take(64)
                        .collect::<String>()
                } else {
                    slug
                },
                &mut used,
            );
            let mut meta = DocMeta::default();
            meta.extra.insert("legacy_id".into(), json!(project_id));
            meta.extra.insert("name".into(), json!(name));
            let content = raw.get_str("description").unwrap_or("").to_string();
            let doc = Document {
                document_id: new_id.clone(),
                category: DocumentCategory::Project,
                folder: None,
                content,
                content_hash: String::new(),
                metadata: meta,
                tags: vec![],
                auto_load: vec![],
                references: vec![],
                seat_id: None,
                created_at: raw
                    .get_datetime("created_at")
                    .ok()
                    .map(|d| d.to_chrono())
                    .unwrap_or(now),
                updated_at: raw
                    .get_datetime("updated_at")
                    .ok()
                    .map(|d| d.to_chrono())
                    .unwrap_or(now),
                version: 1,
                deleted_at: None,
            };
            project_by_id.insert(project_id, new_id);
            jobs.push((doc, false));
        }
    }

    // ── tasks → Task documents (bound to projects via metadata.project) ──
    {
        let tasks: Collection<BsonDoc> = db.collection("tasks");
        let mut cursor = tasks
            .find(doc! {})
            .await
            .map_err(|e| SlcError::Storage(format!("tasks find: {e}")))?;
        while cursor
            .advance()
            .await
            .map_err(|e| SlcError::Storage(format!("cursor: {e}")))?
        {
            let raw: BsonDoc = cursor
                .deserialize_current()
                .map_err(|e| SlcError::Storage(format!("row: {e}")))?;
            let Some(task_id) = raw.get_str("task_id").ok().map(String::from) else {
                continue;
            };
            let slug = slugify(&task_id);
            let new_id = unique(
                if slug.is_empty() {
                    task_id
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .take(64)
                        .collect::<String>()
                } else {
                    slug
                },
                &mut used,
            );
            let mut meta = DocMeta::default();
            meta.extra.insert("legacy_id".into(), json!(task_id));
            for k in ["name", "status", "priority"] {
                if let Ok(v) = raw.get_str(k) {
                    meta.extra.insert(k.into(), json!(v));
                }
            }
            if let Ok(pid) = raw.get_str("project_id") {
                match project_by_id.get(pid) {
                    Some(p) => {
                        meta.extra.insert("project".into(), json!(p));
                    }
                    None => {
                        meta.extra.insert("legacy_project_id".into(), json!(pid));
                    }
                }
            }
            // Body: description; if empty — the original task data survives
            // in metadata (original_data), so dump it as the content.
            let mut content = raw.get_str("description").unwrap_or("").to_string();
            if content.trim().is_empty() {
                if let Ok(od) = raw
                    .get_document("metadata")
                    .and_then(|m| m.get_document("original_data"))
                {
                    content = serde_json::to_string_pretty(&bson_doc_to_map(od)).unwrap_or_default();
                }
            }
            let seat_id = raw.get_str("seat_id").ok().map(String::from);
            let doc = Document {
                document_id: new_id,
                category: DocumentCategory::Task,
                folder: None,
                content,
                content_hash: String::new(),
                metadata: meta,
                tags: vec![],
                auto_load: bson_strs(&raw, "auto_load"),
                references: vec![],
                seat_id,
                created_at: raw
                    .get_datetime("created_at")
                    .ok()
                    .map(|d| d.to_chrono())
                    .unwrap_or(now),
                updated_at: raw
                    .get_datetime("updated_at")
                    .ok()
                    .map(|d| d.to_chrono())
                    .unwrap_or(now),
                version: 1,
                deleted_at: None,
            };
            jobs.push((doc, false));
        }
    }

    // ── knowledge_base → KB documents + episodic diary ──
    let kb: Collection<BsonDoc> = db.collection("knowledge_base");
    let mut cursor = kb
        .find(doc! {})
        .await
        .map_err(|e| SlcError::Storage(format!("knowledge_base find: {e}")))?;
    let mut docs = Vec::new();
    while cursor
        .advance()
        .await
        .map_err(|e| SlcError::Storage(format!("cursor: {e}")))?
    {
        let raw: BsonDoc = cursor
            .deserialize_current()
            .map_err(|e| SlcError::Storage(format!("row: {e}")))?;
        let Some(doc) = extract_legacy_doc(&raw) else { continue };
        if doc.deleted_at.is_some() {
            report.skipped_deleted += 1;
            continue;
        }
        docs.push(doc);
    }
    // Deterministic order → stable ids across runs.
    docs.sort_by_key(|d| d.created_at);

    let mut id_map: HashMap<String, String> = HashMap::new();
    let mut kb_jobs: Vec<(LegacyDoc, String)> = Vec::with_capacity(docs.len());
    for doc in docs {
        if doc_category(&doc) == DocumentCategory::History {
            let new_id = unique(history_id(&doc.document_id, doc.created_at), &mut used);
            id_map.insert(doc.document_id.clone(), new_id.clone());
            kb_jobs.push((doc, new_id));
            continue;
        }
        let ai = if rename_with_ai {
            match llm {
                Some(l) => match tokio::time::timeout(Duration::from_secs(30), ai_slug(l, &doc)).await
                {
                    Ok(Some(slug)) => Some(slug),
                    _ => None,
                },
                None => None,
            }
        } else {
            None
        };
        if ai.is_some() {
            report.renamed_with_ai += 1;
        }
        let new_id = new_kb_id(&doc, ai, &mut used);
        if new_id != doc.document_id {
            id_map.insert(doc.document_id.clone(), new_id.clone());
        }
        kb_jobs.push((doc, new_id));
    }

    // Insert KB docs (links rewritten only after the FULL id map is known).
    for (doc, new_id) in kb_jobs {
        let is_history = doc_category(&doc) == DocumentCategory::History;
        let document = build_migrated_doc(doc, new_id, &id_map, now);
        let res = if is_history {
            target.episodic_upsert(&document).await
        } else {
            target.kb_upsert(&document).await
        };
        match res {
            Ok(_) => {
                if is_history {
                    report.history += 1;
                } else {
                    report.documents += 1;
                }
            }
            Err(e) => report.errors.push(format!("{} ({e})", document.document_id)),
        }
    }

    // ── projects / tasks (planned above) ──
    for (mut document, _) in jobs {
        document.content_hash = content_hash(&document.content);
        document.auto_load = fix_links(&document.auto_load, &id_map);
        document.references = fix_links(&document.references, &id_map);
        match target.kb_upsert(&document).await {
            Ok(_) => {
                if document.category == DocumentCategory::Project {
                    report.projects += 1;
                } else {
                    report.tasks += 1;
                }
            }
            Err(e) => report.errors.push(format!("{} ({e})", document.document_id)),
        }
    }

    // ── seats ──
    let seats: Collection<BsonDoc> = db.collection("seats");
    let mut cursor = seats
        .find(doc! {})
        .await
        .map_err(|e| SlcError::Storage(format!("seats find: {e}")))?;
    while cursor
        .advance()
        .await
        .map_err(|e| SlcError::Storage(format!("cursor: {e}")))?
    {
        let raw: BsonDoc = cursor
            .deserialize_current()
            .map_err(|e| SlcError::Storage(format!("row: {e}")))?;
        let Some(seat) = extract_legacy_seat(&raw) else { continue };
        if let Err(e) = target.insert_seat(&seat).await {
            report.errors.push(format!("seat {}: {e}", seat.seat_id));
        } else {
            report.seats += 1;
        }
    }

    Ok(report)
}

/// Build the final `Document` for a migrated KB doc (folder auto-resolved
/// by the engine, links rewritten through `id_map`).
fn build_migrated_doc(
    doc: LegacyDoc,
    new_id: String,
    id_map: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> Document {
    let cat = doc_category(&doc);
    let legacy_id = doc.document_id.clone();
    let mut meta = DocMeta {
        doc_type: doc.doc_type,
        ..Default::default()
    };
    meta.extra = doc.metadata;
    meta.extra.insert("legacy_id".into(), json!(legacy_id));
    Document {
        document_id: new_id,
        category: cat,
        folder: None,
        content: doc.content,
        content_hash: String::new(),
        metadata: meta,
        tags: doc.tags,
        auto_load: fix_links(&doc.auto_load, id_map),
        references: fix_links(&doc.references, id_map),
        seat_id: doc.seat_id,
        created_at: doc.created_at.unwrap_or(now),
        updated_at: doc.updated_at.unwrap_or(now),
        version: doc.version.max(1),
        deleted_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;
    use std::sync::Arc;

    fn legacy_file(root: &Path, coll: &str, stem: &str, content: &str) {
        let dir = root.join(coll);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{stem}.md")), content).unwrap();
    }

    /// Build a representative legacy vault fixture (per the legacy format).
    fn fixture(root: &Path) {
        legacy_file(
            root,
            "knowledge_base",
            "project_vassista_plan",
            "---\ndocument_id: project_vassista_plan\ncategory: project\nname: Vassista Plan\nseat_id: seat_a\n---\nПлан платформы Vassista",
        );
        legacy_file(
            root,
            "knowledge_base",
            "skill_rust",
            "---\ndocument_id: skill_rust\ncategory: skill\nname: Rust Basics\n---\nКак работать с borrow checker",
        );
        legacy_file(
            root,
            "knowledge_base",
            "note_coffee",
            "---\ndocument_id: note_coffee\ncategory: documentation\n---\nПользователь любит кофе",
        );
        legacy_file(
            root,
            "history",
            "ev_abc",
            "---\ndocument_id: ev_abc\nseat_id: seat_a\n---\nработали над миграцией",
        );
        legacy_file(
            root,
            "focuses",
            "foc_123",
            "---\nfocus_id: foc_123\nseat_id: seat_a\npriority: 3\nname: Миграция SLC\n---\nДовести мигратор до ума",
        );
        legacy_file(
            root,
            "seats",
            "seat_a",
            "---\nseat_id: seat_a\nname: test\n---\n",
        );
        legacy_file(
            root,
            "ideas",
            "idea_x",
            "---\nidea_id: idea_x\n---\nстарая идея",
        );
    }

    #[tokio::test]
    async fn migrate_legacy_vault_to_new_store() {
        let root = std::env::temp_dir().join(format!("slc-legacy-fixture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        fixture(&root);

        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let report = migrate_legacy_vault(&root, store.as_ref()).await.unwrap();

        // 3 KB docs + 1 episodic.
        assert_eq!(report.documents, 4, "{report:?}");
        assert_eq!(report.focuses, 1);
        assert_eq!(report.seats, 1);
        assert_eq!(report.skipped_ideas, 1, "ideas concept is gone");
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        // Human-readable ids: slug from the name/title (here the legacy id
        // already equals the slug, so the same document is found by it).
        // No category prefixes anymore — the folder tree carries context.
        let proj = store.kb_get("vassista_plan").await.unwrap().expect("project doc");
        assert_eq!(proj.category, DocumentCategory::Project);
        let by_new = store
            .kb_find(
                &crate::storage::DocFilter {
                    category: Some(DocumentCategory::Project),
                    ..Default::default()
                },
                &crate::storage::DocSort::by_created(crate::storage::SortDir::Desc),
                10,
            )
            .await
            .unwrap();
        assert_eq!(by_new.len(), 1);
        assert_eq!(by_new[0].document_id, "vassista_plan");
        assert_eq!(by_new[0].content, "План платформы Vassista");
        assert_eq!(by_new[0].seat_id.as_deref(), Some("seat_a"));

        let skill = store.kb_get("rust_basics").await.unwrap().expect("slugged skill id");
        assert_eq!(skill.category, DocumentCategory::Skill);
        assert_eq!(skill.content, "Как работать с borrow checker");

        // Legacy id preserved in metadata.
        assert_eq!(skill.metadata.extra.get("legacy_id").and_then(|v| v.as_str()), Some("skill_rust"));

        // Episodic in the episodic store, not the KB.
        assert_eq!(
            store
                .kb_find(&Default::default(), &crate::storage::DocSort::by_created(crate::storage::SortDir::Desc), 10)
                .await
                .unwrap()
                .len(),
            3
        );
        let ev = store
            .episodic_find(&Default::default(), &crate::storage::DocSort::by_created(crate::storage::SortDir::Desc), 10)
            .await
            .unwrap();
        assert_eq!(ev.len(), 1);
        // Cyrillic body has no ascii slug → deterministic fallback on the
        // legacy id.
        assert_eq!(ev[0].document_id, "history_legacy_ev_abc");

        // Focus record written.
        let focus = store.get_record("focuses", "foc_123").await.unwrap().expect("focus record");
        assert_eq!(focus["title"], "Миграция SLC");
        assert_eq!(focus["priority"], 3);

        // Seat written.
        let seat = store.get_seat("seat_a").await.unwrap().expect("seat");
        assert_eq!(seat.name, "test");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_legacy_md_handles_frontmatter_and_plain() {
        let (fm, body) = parse_legacy_md("---\ndocument_id: x\ncategory: project\n---\nТело");
        assert_eq!(fm.get("document_id").and_then(|v| v.as_str()), Some("x"));
        assert_eq!(body, "Тело");
        let (fm2, body2) = parse_legacy_md("просто текст");
        assert!(fm2.is_empty());
        assert_eq!(body2, "просто текст");
    }

    #[test]
    fn human_id_slugs_names_and_falls_back() {
        let fm: Map<String, Value> = serde_json::from_str(r#"{"name": "Vassista Plan"}"#).unwrap();
        assert_eq!(human_id("project_", &fm, ""), "project_vassista_plan");
        let empty = Map::new();
        assert_eq!(human_id("doc_", &empty, ""), "doc_legacy_doc");
    }

    // ── MongoDB legacy migration ────────────────────────────────────────

    #[test]
    fn bson_to_json_maps_datetimes_and_docs() {
        let dt = DateTime::parse_from_rfc3339("2026-03-10T09:49:34.582000Z")
            .unwrap()
            .with_timezone(&Utc);
        let doc = bson::doc! {
            "s": "text", "i32": 1i32, "i64": 2i64, "f": 1.5f64, "b": true,
            "dt": bson::DateTime::from_chrono(dt),
            "nested": bson::doc! { "k": "v" },
            "arr": ["a", "b"],
            "null": null,
        };
        let j = bson_doc_to_map(&doc);
        assert_eq!(j["s"], "text");
        assert_eq!(j["i32"], 1);
        assert_eq!(j["i64"], 2);
        assert_eq!(j["b"], true);
        assert_eq!(j["dt"], "2026-03-10T09:49:34.582+00:00");
        assert_eq!(j["nested"]["k"], "v");
        assert_eq!(j["arr"], json!(["a", "b"]));
        assert_eq!(j["null"], Value::Null);
    }

    #[test]
    fn extract_legacy_doc_normalizes_content_and_metadata() {
        let dt = bson::DateTime::from_chrono(Utc::now());
        let raw = bson::doc! {
            "document_id": "core_slc_manifest",
            "category": "DOCUMENTATION",          // case-insensitive parse
            "content": bson::doc! { "name": "manifest", "version": 1 },
            "metadata": bson::doc! { "type": "plan", "seed_version": 2, "loaded_at": dt },
            "tags": ["a", "b"],
            "auto_load": ["documentation_xyz"],
            "references": ["custom_abc", "missing_doc"],
            "seat_id": null,
            "version": 3i32,
        };
        let d = extract_legacy_doc(&raw).unwrap();
        assert_eq!(d.document_id, "core_slc_manifest");
        assert_eq!(d.category.as_deref(), Some("DOCUMENTATION"));
        assert_eq!(doc_category(&d), DocumentCategory::Documentation);
        assert!(d.content.contains("\"name\": \"manifest\""));
        assert_eq!(d.doc_type.as_deref(), Some("plan"));
        assert_eq!(d.metadata["seed_version"], 2);
        assert!(d.metadata.get("type").is_none(), "type moved to doc_type");
        assert_eq!(d.tags, vec!["a", "b"]);
        assert_eq!(d.auto_load, vec!["documentation_xyz"]);
        assert_eq!(d.version, 3);
        assert!(d.seat_id.is_none());
        assert!(d.deleted_at.is_none());

        // String content passes through untouched.
        let raw2 = bson::doc! { "document_id": "x", "content": "просто текст" };
        let d2 = extract_legacy_doc(&raw2).unwrap();
        assert_eq!(d2.content, "просто текст");

        // Soft-deleted docs are detectable.
        let raw3 = bson::doc! { "document_id": "y", "deleted_at": dt };
        assert!(extract_legacy_doc(&raw3).unwrap().deleted_at.is_some());
    }

    #[test]
    fn extract_legacy_seat_handles_both_layouts() {
        // Recovered-from-JSON layout: last_activity + statistics + deleted.
        let raw = bson::doc! {
            "seat_id": "recovered_seat_b2aa8d86",
            "name": "test-laptop",
            "status": "deleted",
            "created_at": bson::DateTime::from_chrono(Utc::now()),
            "last_activity": bson::DateTime::from_chrono(Utc::now()),
            "expires_at": bson::DateTime::from_chrono(Utc::now()),
            "metadata": bson::doc! { "migrated_from_json": true },
            "statistics": bson::doc! {
                "total_requests": 7i64, "total_tokens": 12i64,
                "tool_usage": bson::doc! { "search": 3i64 },
            },
            "context": null,
        };
        let s = extract_legacy_seat(&raw).unwrap();
        assert_eq!(s.seat_id, "recovered_seat_b2aa8d86");
        assert_eq!(s.status, SeatStatus::Active, "все сиды активны");
        assert!(s.expires_at.is_none(), "сиды не протухают");
        assert_eq!(s.metadata["legacy_status"], "deleted");
        let exp = s.metadata["legacy_expires_at"].as_str().unwrap();
        assert!(
            DateTime::parse_from_rfc3339(exp).is_ok(),
            "legacy_expires_at должен быть RFC 3339: {exp}"
        );
        assert_eq!(s.metadata["migrated_from_json"], true);
        assert!(s.context.is_empty(), "null context → empty map");
        assert_eq!(s.usage_stats.total_requests, 7);
        assert_eq!(s.usage_stats.total_tokens, 12);
        assert_eq!(s.usage_stats.tools_used["search"], 3);

        // Standard layout: last_accessed + usage_stats + active task.
        let raw2 = bson::doc! {
            "seat_id": "SuvU3sGLTW0fwNlIATtSNgcF6gf0cAo0FXSpeo4adKs",
            "name": "mcp-client",
            "status": "active",
            "created_at": bson::DateTime::from_chrono(Utc::now()),
            "last_accessed": bson::DateTime::from_chrono(Utc::now()),
            "active_task_id": "task_eb36fa40",
            "context": bson::doc! { "context_limit_chars": 8000i64 },
            "usage_stats": bson::doc! { "total_requests": 1237i64, "tools_used": bson::doc! {} },
        };
        let s2 = extract_legacy_seat(&raw2).unwrap();
        assert_eq!(s2.metadata["legacy_active_task_id"], "task_eb36fa40");
        assert!(s2.active_task_id.is_none(), "указатель на легаси-таску в metadata");
        assert_eq!(s2.context["context_limit_chars"], 8000);
        assert_eq!(s2.usage_stats.total_requests, 1237);
        assert!(s2.metadata.get("legacy_status").is_none());
    }

    #[test]
    fn slugify_and_ids_are_unique_and_readable() {
        assert_eq!(slugify("  SLC MCP Server #3! "), "slc_mcp_server_3");
        assert_eq!(slugify("—"), "");
        assert_eq!(slugify("…"), "");

        let mut used = HashSet::new();
        let a = unique("doc_x".into(), &mut used);
        let b = unique("doc_x".into(), &mut used);
        let c = unique("doc_x".into(), &mut used);
        assert_eq!((a.as_str(), b.as_str(), c.as_str()), ("doc_x", "doc_x_2", "doc_x_3"));

        let h = history_id("history_d5ae1bb1a5f52096", None);
        assert!(h.starts_with("history_undated_"), "{h}");
        let h2 = history_id(
            "history_d5ae1bb1a5f52096",
            Some(DateTime::parse_from_rfc3339("2026-03-10T09:49:34Z").unwrap().with_timezone(&Utc)),
        );
        assert_eq!(h2, "history_2026_03_10_1bb1a5f52096", "{h2}");
        assert_ne!(h, h2);
    }

    #[test]
    fn new_kb_id_prefers_ai_slug_and_falls_back() {
        let d = LegacyDoc {
            document_id: "doc_a1b2c3".into(),
            category: Some("custom".into()),
            content: "# План миграции SLC\n\nпереносим память".into(),
            doc_type: None,
            metadata: Map::new(),
            tags: vec![],
            auto_load: vec![],
            references: vec![],
            seat_id: None,
            created_at: None,
            updated_at: None,
            version: 1,
            deleted_at: None,
        };
        let mut used = HashSet::new();
        let ai = new_kb_id(&d, Some("migration_slc_plan".into()), &mut used);
        assert_eq!(ai, "migration_slc_plan", "AI-слаг без категорийного префикса");
        // Без AI: латинское "SLC" в заголовке даёт ascii-slug.
        let fb = new_kb_id(&d, None, &mut used);
        assert_eq!(fb, "slc");
        // Киррилический текст без заголовка — fallback на осмысленный legacy id.
        let d2 = LegacyDoc {
            document_id: "doc_a1b2c3".into(),
            content: "просто кириллический текст".into(),
            ..Default::default()
        };
        let mut used2 = HashSet::new();
        assert_eq!(new_kb_id(&d2, None, &mut used2), "doc_a1b2c3");
    }

    #[tokio::test]
    async fn ai_slug_uses_reasoning_model_and_sanitizes() {
        use crate::llm::MockLlm;
        let llm = MockLlm::new(vec!["  Simple-Name! ".to_string()]);
        let d = LegacyDoc {
            document_id: "doc_x".into(),
            category: Some("module".into()),
            content: "какой-то документ".into(),
            ..Default::default()
        };
        let slug = ai_slug(&llm, &d).await.unwrap();
        assert_eq!(slug, "simple_name");
        // Не-ASCII ответ модели → пустой slug → None (fallback на детерминированный).
        let llm2 = MockLlm::new(vec!["Простое-имя".to_string()]);
        assert!(ai_slug(&llm2, &d).await.is_none());
    }

    #[test]
    fn fix_links_rewrites_known_ids_and_keeps_unknown() {
        let mut map = HashMap::new();
        map.insert("old_a".to_string(), "doc_new_a".to_string());
        map.insert("old_b".to_string(), "doc_new_b".to_string());
        let out = fix_links(&["old_a".into(), "ghost".into(), "old_b".into()], &map);
        assert_eq!(out, vec!["doc_new_a", "ghost", "doc_new_b"]);
    }

    #[test]
    fn hierarchical_default_folders() {
        // Knowledge categories live under docs/ (no prefix, folders carry context).
        assert_eq!(
            Document::new("manifest", DocumentCategory::Core, "x", Default::default(), vec![], None)
                .default_folder(),
            "docs/core"
        );
        assert_eq!(
            Document::new("notes", DocumentCategory::Custom, "x", Default::default(), vec![], None)
                .default_folder(),
            "docs/custom"
        );
        assert_eq!(
            Document::new("guide", DocumentCategory::Documentation, "x", Default::default(), vec![], None)
                .default_folder(),
            "docs"
        );
        assert_eq!(
            Document::new("audit", DocumentCategory::Task, "x", Default::default(), vec![], None)
                .default_folder(),
            "tasks"
        );

        // Project note: docs/projects/<slug>/ (legacy `project_` stripped).
        let proj = Document::new("slc", DocumentCategory::Project, "x", Default::default(), vec![], None);
        assert_eq!(proj.default_folder(), "docs/projects/slc");
        let mut proj2 = Document::new("project_slc", DocumentCategory::Project, "x", Default::default(), vec![], None);
        assert_eq!(proj2.default_folder(), "docs/projects/slc");
        // Explicit metadata.project wins even for a project note.
        let mut m = DocMeta::default();
        m.extra.insert("project".into(), json!("cellframe"));
        proj2.metadata = m;
        assert_eq!(proj2.default_folder(), "docs/projects/cellframe");

        // Docs bound to a project go inside its folder, per-category subfolder.
        let mut meta = DocMeta::default();
        meta.extra.insert("project".into(), json!("slc"));
        let task = Document::new("migration", DocumentCategory::Task, "x", meta.clone(), vec![], None);
        assert_eq!(task.default_folder(), "docs/projects/slc/tasks");
        let note = Document::new("mcp-setup", DocumentCategory::Documentation, "x", meta, vec![], None);
        assert_eq!(note.default_folder(), "docs/projects/slc/docs");

        // project slug is sanitized — cannot escape the projects tree.
        let mut evil = DocMeta::default();
        evil.extra.insert("project".into(), json!("../../etc/passwd"));
        let d = Document::new("x", DocumentCategory::Custom, "x", evil, vec![], None);
        assert_eq!(d.default_folder(), "docs/projects/etc_passwd/custom");
    }

    #[test]
    fn build_migrated_doc_fixes_links_and_keeps_binding() {
        let mut id_map = HashMap::new();
        id_map.insert("documentation_abc".into(), "mcp_setup".into());
        let doc = LegacyDoc {
            document_id: "custom_old".into(),
            category: Some("documentation".into()),
            content: "текст".into(),
            doc_type: Some("plan".into()),
            metadata: {
                let mut m = Map::new();
                m.insert("project".into(), json!("slc"));
                m
            },
            tags: vec!["x".into()],
            auto_load: vec!["documentation_abc".into(), "missing".into()],
            references: vec![],
            seat_id: None,
            created_at: None,
            updated_at: None,
            version: 2,
            deleted_at: None,
        };
        let out = build_migrated_doc(doc, "mcp_setup".into(), &id_map, Utc::now());
        assert_eq!(out.document_id, "mcp_setup");
        assert_eq!(out.auto_load, vec!["mcp_setup", "missing"]);
        assert_eq!(out.metadata.doc_type.as_deref(), Some("plan"));
        assert_eq!(out.metadata.extra["legacy_id"], "custom_old");
        assert_eq!(out.metadata.extra["project"], "slc", "привязка к проекту сохраняется");
        assert_eq!(out.version, 2);
        assert_eq!(out.default_folder(), "docs/projects/slc/docs");
    }

    impl Default for LegacyDoc {
        fn default() -> Self {
            LegacyDoc {
                document_id: String::new(),
                category: None,
                content: String::new(),
                doc_type: None,
                metadata: Map::new(),
                tags: Vec::new(),
                auto_load: Vec::new(),
                references: Vec::new(),
                seat_id: None,
                created_at: None,
                updated_at: None,
                version: 1,
                deleted_at: None,
            }
        }
    }
}
