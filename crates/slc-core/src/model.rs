//! Core data model — the UNIFIED `Document` entity, embeddings, seats, timers.
//!
//! Design rules (user-approved):
//!
//! 1. **One entity**: projects, tasks and knowledge documents are ALL
//!    `Document`s (differ by `category` + tags/metadata) — no separate
//!    collections. Document ids are **unique names**, not content hashes
//!    (the hash is stored separately in `content_hash` for dedup).
//! 2. **Episodic history is NOT part of the RAG store**: HISTORY docs live
//!    in a separate episodic store (`StorageBackend::episodic_*`), never get
//!    embedded, never appear in knowledge search. They are processed and
//!    queried by their own pipeline (progressive summarization L1→L4) and
//!    their own retrieval.
//! 3. Knowledge search = hybrid (BM25 + cosine) over KB documents only.

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};

/// Unified document categories. `Project` and `Task` are documents like any
/// other — they just carry task/project semantics in metadata/tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentCategory {
    Core,
    Module,
    Task,
    Project,
    History,
    CodeSnippet,
    Documentation,
    /// A skill — the same unified `Document`, just named a skill
    /// (procedural knowledge: how to do things).
    Skill,
    Custom,
    System,
}

impl DocumentCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentCategory::Core => "core",
            DocumentCategory::Module => "module",
            DocumentCategory::Task => "task",
            DocumentCategory::Project => "project",
            DocumentCategory::History => "history",
            DocumentCategory::CodeSnippet => "code_snippet",
            DocumentCategory::Documentation => "documentation",
            DocumentCategory::Skill => "skill",
            DocumentCategory::Custom => "custom",
            DocumentCategory::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "core" => Some(Self::Core),
            "module" => Some(Self::Module),
            "task" => Some(Self::Task),
            "project" => Some(Self::Project),
            "history" | "episodic" => Some(Self::History),
            "code_snippet" => Some(Self::CodeSnippet),
            "documentation" => Some(Self::Documentation),
            "skill" | "skills" => Some(Self::Skill),
            "custom" => Some(Self::Custom),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    /// Categories that are RAG-eligible (never `History`).
    pub fn is_kb(self) -> bool {
        !matches!(self, DocumentCategory::History)
    }
}

/// Episodic memory levels (progressive summarization ladder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DocLevel {
    L1,
    L2,
    L3,
    L4,
}

impl DocLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            DocLevel::L1 => "L1",
            DocLevel::L2 => "L2",
            DocLevel::L3 => "L3",
            DocLevel::L4 => "L4",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "L1" => Some(Self::L1),
            "L2" => Some(Self::L2),
            "L3" => Some(Self::L3),
            "L4" => Some(Self::L4),
            _ => None,
        }
    }
}

/// Typed sub-structure of the free-form `metadata` map (the memory pipeline
/// depends on these fields, so they are typed; everything else goes to
/// `extra`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocMeta {
    #[serde(rename = "doc_type")]
    pub doc_type: Option<String>,
    #[serde(rename = "doc_level")]
    pub doc_level: Option<DocLevel>,
    #[serde(rename = "seat_id")]
    pub seat_id: Option<String>,
    pub archived: Option<bool>,
    #[serde(rename = "compression_batch_id")]
    pub compression_batch_id: Option<String>,
    pub consolidated: Option<bool>,
    pub importance: Option<f64>,
    pub source: Option<String>,
    #[serde(rename = "source_count")]
    pub source_count: Option<i64>,
    pub date: Option<String>,
    /// Task/project extras (status, progress, due, …) land here.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// THE unified entity: knowledge documents, tasks, projects, episodic
/// history — all this type. `document_id` is a **unique name** chosen by the
/// caller (or `unique_id(prefix)`); `content_hash` is only for dedup.
///
/// In the Obsidian vault each document is `{folder}/{document_id}.md` —
/// human-readable folder path + human-readable file name (the id IS the
/// file name); the exact id is also kept in the frontmatter for roundtrip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Unique NAME (not a content hash): e.g. `core_slc_manifest`,
    /// `task_xyz`, `project_abc`, `episodic_l2_{seat}_{date}`.
    pub document_id: String,
    pub category: DocumentCategory,
    /// Relative vault folder (human-organized, e.g. `projects/vassista`,
    /// `knowledge/rust`). `None` = default by category (`core`, `modules`,
    /// `tasks`, `projects`, `knowledge`, …; episodic → `history/{y}/{m}`).
    pub folder: Option<String>,
    pub content: String,
    pub content_hash: String,
    pub metadata: DocMeta,
    pub tags: Vec<String>,
    /// Hyperlinks auto-followed on context-load.
    #[serde(default)]
    pub auto_load: Vec<String>,
    #[serde(default)]
    pub references: Vec<String>,
    /// Owner; `None` = public.
    pub seat_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub version: i64,
    /// Present = soft-deleted (graveyard). History docs are hard-purged by
    /// the episodic pipeline instead (archived L1/L2 are consumed, not kept).
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Document {
    pub fn new(
        document_id: impl Into<String>,
        category: DocumentCategory,
        content: impl Into<String>,
        metadata: DocMeta,
        tags: Vec<String>,
        seat_id: Option<String>,
    ) -> Self {
        Document::with_folder(
            document_id,
            category,
            None,
            content,
            metadata,
            tags,
            seat_id,
        )
    }

    pub fn with_folder(
        document_id: impl Into<String>,
        category: DocumentCategory,
        folder: Option<String>,
        content: impl Into<String>,
        metadata: DocMeta,
        tags: Vec<String>,
        seat_id: Option<String>,
    ) -> Self {
        let now = Utc::now();
        let content = content.into();
        Document {
            document_id: document_id.into(),
            category,
            folder,
            content_hash: content_hash(&content),
            content,
            metadata,
            tags,
            auto_load: Vec::new(),
            references: Vec::new(),
            seat_id,
            created_at: now,
            updated_at: now,
            version: 1,
            deleted_at: None,
        }
    }

    /// Default folder for a document (used when `folder` is `None`).
    ///
    /// Hierarchical layout (user-approved): knowledge categories live under
    /// `docs/`, projects are folders `docs/projects/<project>/` with their
    /// own note and per-category subfolders, tasks outside projects go to
    /// `tasks/`. A document bound to a project
    /// (`metadata.extra["project"] = <slug>`) is placed inside that project's
    /// folder. The vault stays human-organized; indexing/RAG are separate
    /// sidecars (`.slc/`), never mixed into the notes tree.
    pub fn default_folder(&self) -> String {
        if self.category == DocumentCategory::History {
            // Human diary layout: history/YYYY/MM/
            let d = self.created_at;
            return format!("history/{:04}/{:02}", d.year(), d.month());
        }
        if self.category == DocumentCategory::Project {
            // The project note itself: docs/projects/<slug>/ — the slug comes
            // from the document id (legacy `project_` prefix stripped).
            let slug = self
                .project_slug()
                .or_else(|| {
                    slugify_ascii(
                        self.document_id
                            .strip_prefix("project_")
                            .unwrap_or(&self.document_id),
                    )
                })
                .unwrap_or_else(|| "project".into());
            return format!("docs/projects/{slug}");
        }
        if let Some(project) = self.project_slug() {
            // Document of a project (tasks, docs, custom notes, …) → inside
            // the project folder, per-category subfolder.
            return format!("docs/projects/{project}/{}", self.category_dir());
        }
        self.category_folder()
    }

    /// The `docs/<category>` folder for non-project documents.
    fn category_folder(&self) -> String {
        match self.category {
            DocumentCategory::Core => "docs/core".into(),
            DocumentCategory::Module => "docs/modules".into(),
            DocumentCategory::Task => "tasks".into(),
            DocumentCategory::CodeSnippet => "code".into(),
            DocumentCategory::Documentation => "docs".into(),
            DocumentCategory::Skill => "docs/skills".into(),
            DocumentCategory::Custom => "docs/custom".into(),
            DocumentCategory::System => "system".into(),
            DocumentCategory::Project | DocumentCategory::History => {
                unreachable!("project/history folders handled in default_folder")
            }
        }
    }

    /// Leaf directory used INSIDE a project folder (`docs/projects/<p>/<leaf>`).
    fn category_dir(&self) -> &'static str {
        match self.category {
            DocumentCategory::Core => "core",
            DocumentCategory::Module => "modules",
            DocumentCategory::Task => "tasks",
            DocumentCategory::CodeSnippet => "code",
            DocumentCategory::Documentation => "docs",
            DocumentCategory::Skill => "skills",
            DocumentCategory::Custom => "custom",
            DocumentCategory::System => "system",
            DocumentCategory::Project | DocumentCategory::History => {
                unreachable!("project/history folders handled in default_folder")
            }
        }
    }

    /// Project binding from `metadata.extra["project"]` (a lowercase slug,
    /// e.g. `slc`, `confcall`) — sanitized so it can never escape the
    /// `docs/projects/` tree.
    pub fn project_slug(&self) -> Option<String> {
        slugify_ascii(
            self.metadata
                .extra
                .get("project")
                .and_then(|v| v.as_str())?,
        )
    }
}

/// A vector record for a document chunk (chunked embedding storage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    pub document_id: String,
    pub chunk_index: i64,
    pub chunk_total: i64,
    pub embedding: Vec<f32>,
    pub embedding_model: String,
    pub embedding_dimension: usize,
    pub generated_at: DateTime<Utc>,
    pub scope: EmbeddingScope,
    pub seat_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingScope {
    Public,
    Private,
}

/// Multi-seat isolation: each client owns its documents and context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seat {
    pub seat_id: String,
    pub name: String,
    pub status: SeatStatus,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub active_task_id: Option<String>,
    /// The active document (any category) — the "context anchor" of the
    /// seat: it is included in `update_context` and its auto_load links are
    /// followed. Task activation also writes this field (unified).
    pub active_document_id: Option<String>,
    pub context: serde_json::Map<String, serde_json::Value>,
    pub usage_stats: UsageStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeatStatus {
    Active,
    Idle,
    Closed,
    Expired,
}

impl SeatStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SeatStatus::Active => "active",
            SeatStatus::Idle => "idle",
            SeatStatus::Closed => "closed",
            SeatStatus::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "closed" => SeatStatus::Closed,
            "expired" => SeatStatus::Expired,
            "idle" => SeatStatus::Idle,
            _ => SeatStatus::Active,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub total_requests: i64,
    pub total_tokens: i64,
    pub tools_used: serde_json::Map<String, serde_json::Value>,
}

/// Persisted background-job timer (history compression, consolidation, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedTimer {
    pub timer_id: String,
    pub seat_id: String,
    pub timer_type: TimerType,
    pub interval_seconds: Option<i64>,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub next_fire_at: DateTime<Utc>,
    pub is_active: bool,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimerType {
    FocusReminder,
    Consolidation,
    HistoryCompression,
    Reminder,
}

impl TimerType {
    pub fn as_str(self) -> &'static str {
        match self {
            TimerType::FocusReminder => "FOCUS_REMINDER",
            TimerType::Consolidation => "CONSOLIDATION",
            TimerType::HistoryCompression => "HISTORY_COMPRESSION",
            TimerType::Reminder => "REMINDER",
        }
    }
}

/// A user-created reminder — surfaces a message at `remind_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    pub reminder_id: String,
    pub seat_id: String,
    pub mind_type: crate::proactivity::MindType,
    pub user_id: Option<String>,
    pub content: String,
    pub remind_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// `pending` | `fired` | `cancelled`.
    pub status: String,
    /// Optional cron string (unused for now; future recurrence).
    pub recurrence: Option<String>,
    pub created_by_agent: bool,
}

/// A queued notification destined for a seat/agent (the UX channel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub notification_id: String,
    pub seat_id: String,
    /// `REMINDER` | `FOCUS_REMINDER` | `IDEA_REMINDER` | `REFLECTION` | …
    pub source: String,
    pub title: String,
    pub body: String,
    /// `pending` | `delivered` | `dismissed`.
    pub status: String,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
}

/// Lowercase ascii slug (words joined with `_`, ≤48 chars); `None` when the
/// result would be empty. Used for folder segments so user/LLM-supplied
/// project names can never escape the intended tree.
pub fn slugify_ascii(raw: &str) -> Option<String> {
    let slug: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let slug = slug.chars().take(48).collect::<String>();
    if slug.is_empty() { None } else { Some(slug) }
}

/// sha256 hex of the document content — dedup / content identity only.
pub fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

/// Unique NAME-based id: `{prefix}_{uuid12}` (e.g. `tmr_…`, `seat_…`).
/// Callers may pass their own unique names instead (tasks, projects,
/// daily summaries — anything human-meaningful).
pub fn unique_id(prefix: &str) -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &u[..12])
}

/// Cyrillic → Latin transliteration (for readable ids from Russian names).
fn translit_char(c: char, upper: bool) -> Option<String> {
    const LOWER: &[(&str, &str)] = &[
        ("а", "a"),
        ("б", "b"),
        ("в", "v"),
        ("г", "g"),
        ("д", "d"),
        ("е", "e"),
        ("ё", "yo"),
        ("ж", "zh"),
        ("з", "z"),
        ("и", "i"),
        ("й", "y"),
        ("к", "k"),
        ("л", "l"),
        ("м", "m"),
        ("н", "n"),
        ("о", "o"),
        ("п", "p"),
        ("р", "r"),
        ("с", "s"),
        ("т", "t"),
        ("у", "u"),
        ("ф", "f"),
        ("х", "h"),
        ("ц", "ts"),
        ("ч", "ch"),
        ("ш", "sh"),
        ("щ", "sch"),
        ("ъ", ""),
        ("ы", "y"),
        ("ь", ""),
        ("э", "e"),
        ("ю", "yu"),
        ("я", "ya"),
    ];
    let lower: String = c.to_lowercase().collect();
    for (ru, lat) in LOWER {
        if ru == &lower {
            let out = if upper {
                let mut ch = lat.chars();
                match ch.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
                    None => String::new(),
                }
            } else {
                (*lat).to_string()
            };
            return Some(out);
        }
    }
    None
}

/// Human-readable slug from a name: Cyrillic transliteration, lowercase,
/// non-letters → `_`, empty segments collapse, up to 48 characters. If the
/// result is empty, `task`-style callers should use a unique_id instead.
pub fn slug_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if let Some(t) = translit_char(c, c.is_uppercase()) {
            out.push_str(&t);
        } else {
            out.push('_');
        }
    }
    let slug: String = out
        .to_lowercase()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    slug.chars().take(48).collect()
}
