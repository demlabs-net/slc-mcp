//! Tasks and projects — modeled as unified `Document`s (category `Task` /
//! `Project`), per the user-approved design rule: "проекты, задачи и
//! документы — всё это документы". Task/project-specific state (status,
//! project link, auto_load) lives in `Document::metadata.extra` and the
//! typed `auto_load` field.
//!
//! Legacy equivalent: the Mongo `tasks` / `projects` collections behind
//! `src/slc_mcp/tools/http/tasks.py` and `projects.py`.

use crate::error::{SlcError, SlcResult};
use crate::model::{DocMeta, Document, DocumentCategory, content_hash};
use crate::storage::{DocFilter, DocSort, SortDir, StorageBackend};
use chrono::Utc;
use serde_json::{Value, json};

/// Canonical status values shared by tasks and projects (SCREAMING_SNAKE,
/// same set the Mongo migration normalizes legacy statuses into).
pub const STATUS_PENDING: &str = "PENDING";
pub const STATUS_ACTIVE: &str = "IN_WORK";
pub const STATUS_COMPLETED: &str = "COMPLETED";
pub const STATUS_BLOCKED: &str = "BLOCKED";
pub const STATUS_FAILED: &str = "FAILED";
pub const STATUS_CANCELLED: &str = "CANCELLED";
pub const STATUS_ARCHIVED: &str = "ARCHIVED";

/// Project lifecycle statuses (separate from task statuses).
pub const STATUS_PROJECT_ACTIVE: &str = "active";
pub const STATUS_PROJECT_ARCHIVED: &str = "archived";

/// Normalize a raw task status into the canonical set; unknown → `None`.
pub fn normalize_task_status(raw: &str) -> Option<&'static str> {
    let norm: String = raw
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    match norm.as_str() {
        "active" | "inprogress" | "inwork" | "running" | "started" | "wip" | "doing" => {
            Some(STATUS_ACTIVE)
        }
        "pending" | "planned" | "backlog" | "queued" | "scheduled" | "open" | "new" => {
            Some(STATUS_PENDING)
        }
        "completed" | "done" | "closed" | "finished" | "resolved" | "merged" | "released" => {
            Some(STATUS_COMPLETED)
        }
        "blocked" | "waiting" | "stalled" | "onhold" => Some(STATUS_BLOCKED),
        "failed" | "error" | "errored" => Some(STATUS_FAILED),
        "cancelled" | "canceled" | "rejected" | "abandoned" | "wontfix" => Some(STATUS_CANCELLED),
        _ => None,
    }
}

/// Task/Project shape returned by the CRUD tools.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskInfo {
    pub task_id: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub project_id: Option<String>,
    pub auto_load: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Stable workflow principal that created/delegated this task.
    pub issuer: Option<String>,
    /// Stable workflow principal that owns execution of this task.
    pub assignee: Option<String>,
    pub parent_task_id: Option<String>,
    pub root_task_id: Option<String>,
    pub last_event_id: Option<String>,
    pub last_event_at: Option<String>,
    pub terminal_summary: Option<String>,
    /// Durable per-assignee execution lane state. Workflow tasks are queued
    /// FIFO and at most one task may be `ready` or `running` for a principal.
    pub queue_state: Option<String>,
    /// Stable FIFO key assigned once when the workflow task is created.
    pub queue_order: Option<String>,
    /// Event used as the idempotency key for the current runnable wake.
    pub queue_ready_event_id: Option<String>,
    /// Caller-owned structured task data, isolated from SLC projection fields.
    pub metadata: Value,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProjectInfo {
    pub project_id: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub auto_load: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn extra_field(doc: &Document, key: &str) -> Option<String> {
    doc.metadata
        .extra
        .get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Whether a task participates in the durable delegated-workflow contract.
/// Such tasks are append-only through workflow events; legacy CRUD must not
/// rewrite identity, lineage, status, or canonical task content behind the
/// event stream.
pub fn is_workflow_task(doc: &Document) -> bool {
    doc.category == DocumentCategory::Task
        && doc
            .metadata
            .extra
            .get("workflow_version")
            .and_then(Value::as_u64)
            .is_some()
}

pub(crate) fn doc_to_task(doc: &Document) -> TaskInfo {
    TaskInfo {
        task_id: doc.document_id.clone(),
        name: extra_field(doc, "name").unwrap_or_else(|| doc.document_id.clone()),
        description: doc.content.clone(),
        status: extra_field(doc, "status").unwrap_or_else(|| STATUS_PENDING.into()),
        // Канонический ключ — "project"; "project_id" остаётся для старых доков.
        project_id: extra_field(doc, "project").or_else(|| extra_field(doc, "project_id")),
        auto_load: doc.auto_load.clone(),
        created_at: doc.created_at.to_rfc3339(),
        updated_at: doc.updated_at.to_rfc3339(),
        issuer: extra_field(doc, "issuer"),
        assignee: extra_field(doc, "assignee"),
        parent_task_id: extra_field(doc, "parent_task_id"),
        root_task_id: extra_field(doc, "root_task_id").or_else(|| Some(doc.document_id.clone())),
        last_event_id: extra_field(doc, "last_event_id"),
        last_event_at: extra_field(doc, "last_event_at"),
        terminal_summary: extra_field(doc, "terminal_summary"),
        queue_state: extra_field(doc, "queue_state"),
        queue_order: extra_field(doc, "queue_order"),
        queue_ready_event_id: extra_field(doc, "queue_ready_event_id"),
        metadata: doc
            .metadata
            .extra
            .get("workflow_metadata")
            .cloned()
            .unwrap_or_else(|| Value::Object(doc.metadata.extra.clone())),
    }
}

fn doc_to_project(doc: &Document) -> ProjectInfo {
    ProjectInfo {
        project_id: doc.document_id.clone(),
        name: extra_field(doc, "name").unwrap_or_else(|| doc.document_id.clone()),
        description: doc.content.clone(),
        status: extra_field(doc, "status").unwrap_or_else(|| STATUS_PROJECT_ACTIVE.into()),
        auto_load: doc.auto_load.clone(),
        created_at: doc.created_at.to_rfc3339(),
        updated_at: doc.updated_at.to_rfc3339(),
    }
}

/// Build the unified Document backing a task or project.
fn build_doc(
    document_id: String,
    category: DocumentCategory,
    name: &str,
    description: &str,
    status: &str,
    project_id: Option<&str>,
    auto_load: &[String],
    metadata: &Value,
    seat_id: &str,
) -> Document {
    let mut meta = DocMeta::default();
    meta.seat_id = Some(seat_id.into());
    meta.extra.insert("name".into(), json!(name));
    meta.extra.insert("status".into(), json!(status));
    if let Some(pid) = project_id {
        // "project" — канонический ключ: из него вычисляется папка
        // (docs/projects/<p>/tasks/) и он же отдаётся в TaskInfo.
        meta.extra.insert("project".into(), json!(pid));
    }
    if let Some(obj) = metadata.as_object() {
        for (k, v) in obj {
            meta.extra.insert(k.clone(), v.clone());
        }
    }
    let mut doc = Document::with_folder(
        document_id,
        category,
        None,
        description,
        meta,
        vec!["work_item".into()],
        Some(seat_id.into()),
    );
    doc.auto_load = auto_load.to_vec();
    doc
}

/// CRUD manager over task/project Documents.
#[derive(Clone)]
pub struct WorkItemManager<S: StorageBackend> {
    store: S,
}

impl<S: StorageBackend> WorkItemManager<S> {
    pub fn new(store: S) -> Self {
        WorkItemManager { store }
    }

    // ── tasks ─────────────────────────────────────────────────────

    pub async fn create_task(
        &self,
        seat_id: &str,
        name: &str,
        description: &str,
        project_id: Option<&str>,
        auto_load: &[String],
        metadata: &Value,
    ) -> SlcResult<TaskInfo> {
        let task_id = self.speaking_id(name).await;
        let doc = build_doc(
            task_id.clone(),
            DocumentCategory::Task,
            name,
            description,
            STATUS_PENDING,
            project_id,
            auto_load,
            metadata,
            seat_id,
        );
        self.store.kb_insert(&doc).await?;
        Ok(doc_to_task(&doc))
    }

    /// Говорящий id — транслит названия БЕЗ категорийного префикса
    /// (папка уже несёт категорию: `tasks/`, `docs/projects/<p>/tasks/`),
    /// при коллизии — `_2`, `_3`…; пустой слаг — unique_id fallback.
    pub(crate) async fn speaking_id(&self, name: &str) -> String {
        let slug = crate::model::slug_name(name);
        if slug.is_empty() {
            return crate::model::unique_id("task");
        }
        let mut id = slug.clone();
        let mut n = 1usize;
        while self
            .store
            .kb_get(&id)
            .await
            .map(|d| d.is_some())
            .unwrap_or(true)
        {
            n += 1;
            id = format!("{slug}_{n}");
            if n > 100 {
                return crate::model::unique_id("task");
            }
        }
        id
    }

    pub async fn get_task(&self, seat_id: &str, task_id: &str) -> SlcResult<Option<TaskInfo>> {
        let Some(doc) = self.store.kb_get(task_id).await? else {
            return Ok(None);
        };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Task {
            return Ok(None);
        }
        Ok(Some(doc_to_task(&doc)))
    }

    pub async fn update_task(
        &self,
        seat_id: &str,
        task_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        description_patch: Option<&Value>,
        project_id: Option<Option<&str>>,
        auto_load: Option<&[String]>,
        status: Option<&str>,
        metadata: Option<&Value>,
    ) -> SlcResult<Option<TaskInfo>> {
        let Some(mut doc) = self.store.kb_get(task_id).await? else {
            return Ok(None);
        };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Task {
            return Ok(None);
        }
        if is_workflow_task(&doc) {
            return Err(SlcError::InvalidInput(
                "workflow tasks are append-only; use task_message, report_task, or cancel_task"
                    .into(),
            ));
        }
        if let Some(n) = name {
            doc.metadata.extra.insert("name".into(), json!(n));
        }
        if let Some(d) = description {
            doc.content = d.into();
            doc.content_hash = content_hash(&doc.content);
        }
        if let Some(patch) = description_patch {
            // Инкрементальное обновление вместо пересылки всего тела.
            doc.content =
                apply_description_patch(&doc.content, patch).map_err(SlcError::InvalidInput)?;
            doc.content_hash = content_hash(&doc.content);
        }
        if let Some(Some(pid)) = project_id {
            doc.metadata.extra.insert("project".into(), json!(pid));
        }
        if let Some(pid) = project_id {
            if pid.is_none() {
                doc.metadata.extra.remove("project");
                doc.metadata.extra.remove("project_id");
            }
        }
        if let Some(al) = auto_load {
            doc.auto_load = al.to_vec();
        }
        if let Some(st) = status {
            // Статус нормализуется в канонический набор; неизвестный — не трогаем.
            if let Some(norm) = normalize_task_status(st) {
                doc.metadata.extra.insert("status".into(), json!(norm));
            }
        }
        if let Some(obj) = metadata.and_then(|m| m.as_object()) {
            for (k, v) in obj {
                doc.metadata.extra.insert(k.clone(), v.clone());
            }
        }
        doc.updated_at = Utc::now();
        doc.version += 1;
        self.store.kb_replace(&doc).await?;
        Ok(Some(doc_to_task(&doc)))
    }

    pub async fn delete_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let Some(doc) = self.store.kb_get(task_id).await? else {
            return Ok(false);
        };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Task {
            return Ok(false);
        }
        if is_workflow_task(&doc) {
            return Err(SlcError::InvalidInput(
                "workflow tasks cannot be deleted; preserve their event history".into(),
            ));
        }
        self.store.kb_purge(task_id).await
    }

    pub async fn list_tasks(
        &self,
        seat_id: &str,
        project_id: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> SlcResult<Vec<TaskInfo>> {
        let filter = DocFilter {
            category: Some(DocumentCategory::Task),
            seat_id: Some(seat_id.into()),
            ..Default::default()
        };
        let docs = self
            .store
            .kb_find(&filter, &DocSort::by_updated(SortDir::Desc), 500)
            .await?;
        let mut out: Vec<TaskInfo> = docs
            .into_iter()
            .filter(|d| d.is_kb_visible(seat_id))
            .map(|d| doc_to_task(&d))
            .filter(|t| {
                project_id
                    .map(|p| t.project_id.as_deref() == Some(p))
                    .unwrap_or(true)
            })
            .filter(|t| status.map(|s| t.status == s).unwrap_or(true))
            .collect();
        out.truncate(limit);
        Ok(out)
    }

    /// Set the seat's active task pointer (working-memory context).
    pub async fn set_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        if self.get_task(seat_id, task_id).await?.is_none() {
            return Ok(false);
        }
        self.store.set_seat_active_task(seat_id, task_id).await
    }

    pub async fn get_active_task(&self, seat_id: &str) -> SlcResult<Option<TaskInfo>> {
        let Some(seat) = self.store.get_seat(seat_id).await? else {
            return Ok(None);
        };
        let Some(task_id) = seat.active_task_id else {
            return Ok(None);
        };
        self.get_task(seat_id, &task_id).await
    }

    // ── projects ──────────────────────────────────────────────────

    pub async fn create_project(
        &self,
        seat_id: &str,
        name: &str,
        description: &str,
        auto_load: &[String],
        metadata: &Value,
    ) -> SlcResult<ProjectInfo> {
        let project_id = self.speaking_id(name).await;
        let doc = build_doc(
            project_id.clone(),
            DocumentCategory::Project,
            name,
            description,
            STATUS_PROJECT_ACTIVE,
            None,
            auto_load,
            metadata,
            seat_id,
        );
        self.store.kb_insert(&doc).await?;
        Ok(doc_to_project(&doc))
    }

    pub async fn get_project(
        &self,
        seat_id: &str,
        project_id: &str,
    ) -> SlcResult<Option<ProjectInfo>> {
        let Some(doc) = self.store.kb_get(project_id).await? else {
            return Ok(None);
        };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Project {
            return Ok(None);
        }
        Ok(Some(doc_to_project(&doc)))
    }

    pub async fn update_project(
        &self,
        seat_id: &str,
        project_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        description_patch: Option<&Value>,
        auto_load: Option<&[String]>,
        status: Option<&str>,
        metadata: Option<&Value>,
    ) -> SlcResult<Option<ProjectInfo>> {
        let Some(mut doc) = self.store.kb_get(project_id).await? else {
            return Ok(None);
        };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Project {
            return Ok(None);
        }
        if let Some(n) = name {
            doc.metadata.extra.insert("name".into(), json!(n));
        }
        if let Some(d) = description {
            doc.content = d.into();
            doc.content_hash = content_hash(&doc.content);
        }
        if let Some(patch) = description_patch {
            doc.content =
                apply_description_patch(&doc.content, patch).map_err(SlcError::InvalidInput)?;
            doc.content_hash = content_hash(&doc.content);
        }
        if let Some(al) = auto_load {
            doc.auto_load = al.to_vec();
        }
        if let Some(st) = status {
            doc.metadata.extra.insert("status".into(), json!(st));
        }
        if let Some(obj) = metadata.and_then(|m| m.as_object()) {
            for (k, v) in obj {
                doc.metadata.extra.insert(k.clone(), v.clone());
            }
        }
        doc.updated_at = Utc::now();
        doc.version += 1;
        self.store.kb_replace(&doc).await?;
        Ok(Some(doc_to_project(&doc)))
    }

    pub async fn delete_project(&self, seat_id: &str, project_id: &str) -> SlcResult<bool> {
        let Some(doc) = self.store.kb_get(project_id).await? else {
            return Ok(false);
        };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Project {
            return Ok(false);
        }
        self.store.kb_purge(project_id).await
    }

    pub async fn list_projects(
        &self,
        seat_id: &str,
        status: Option<&str>,
        limit: usize,
    ) -> SlcResult<Vec<ProjectInfo>> {
        let filter = DocFilter {
            category: Some(DocumentCategory::Project),
            seat_id: Some(seat_id.into()),
            ..Default::default()
        };
        let docs = self
            .store
            .kb_find(&filter, &DocSort::by_updated(SortDir::Desc), 500)
            .await?;
        let mut out: Vec<ProjectInfo> = docs
            .into_iter()
            .filter(|d| d.is_kb_visible(seat_id))
            .map(|d| doc_to_project(&d))
            .filter(|p| status.map(|s| p.status == s).unwrap_or(true))
            .collect();
        out.truncate(limit);
        Ok(out)
    }
}

// helper on Document for seat-visibility of KB docs (extension trait)
impl crate::model::Document {
    pub(crate) fn is_kb_visible(&self, seat_id: &str) -> bool {
        match &self.seat_id {
            None => true,
            Some(owner) => owner == seat_id,
        }
    }
}

/// Инкрементальное обновление markdown-тела (задача/проект) без пересылки
/// всего текста. Операции применяются по порядку:
/// - `{"op":"append","content":"…"}` — добавить в конец;
/// - `{"op":"prepend","content":"…"}` — добавить в начало;
/// - `{"op":"replace_section","heading":"### Фаза 2","content":"…"}` —
///   заменить КОНТЕНТ секции (от заголовка до следующего заголовка того же
///   или более высокого уровня); заголовок сохраняется;
/// - `{"op":"remove_section","heading":"…"}` — удалить секцию целиком
///   (вместе с заголовком).
pub fn apply_description_patch(body: &str, patch: &Value) -> Result<String, String> {
    let Some(ops) = patch.as_array() else {
        return Err("description_patch must be an array of operations".into());
    };
    let mut out = body.to_string();
    for (i, op) in ops.iter().enumerate() {
        let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let content = op.get("content").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "append" => {
                if !out.trim_end().is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(content.trim());
            }
            "prepend" => {
                let c = content.trim();
                if !c.is_empty() {
                    out = if out.trim().is_empty() {
                        c.to_string()
                    } else {
                        format!("{c}\n\n{out}")
                    };
                }
            }
            "replace_section" => {
                let heading = op
                    .get("heading")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("op[{i}]: replace_section requires heading"))?;
                out = apply_section_op(&out, heading, content, true)?;
            }
            "remove_section" => {
                let heading = op
                    .get("heading")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("op[{i}]: remove_section requires heading"))?;
                out = apply_section_op(&out, heading, "", false)?;
            }
            other => return Err(format!("op[{i}]: unknown patch op `{other}`")),
        }
    }
    Ok(out)
}

/// Замена/удаление markdown-секции по заголовку. Секция тянется от
/// заголовка до следующего заголовка ТОГО ЖЕ или более высокого уровня
/// (или конца текста). `keep_heading=false` удаляет и заголовок.
fn apply_section_op(
    body: &str,
    heading: &str,
    new_content: &str,
    keep_heading: bool,
) -> Result<String, String> {
    let target = heading.trim();
    let h_level = target.chars().take_while(|c| *c == '#').count();
    if h_level == 0 || !target[h_level..].starts_with(char::is_whitespace) {
        return Err(format!("heading must be a markdown heading: `{heading}`"));
    }
    let heading_level = |line: &str| -> Option<usize> {
        let t = line.trim_start();
        let lvl = t.chars().take_while(|c| *c == '#').count();
        if lvl > 0 && (t.len() == lvl || t[lvl..].starts_with(char::is_whitespace)) {
            Some(lvl)
        } else {
            None
        }
    };
    let mut out = String::with_capacity(body.len());
    let mut skipping = false;
    let mut found = false;
    for line in body.lines() {
        if let Some(lvl) = heading_level(line) {
            if lvl <= h_level {
                // Заголовок того же/высшего уровня закрывает любую секцию.
                skipping = false;
                if line.trim_start() == target {
                    found = true;
                    skipping = true;
                    if keep_heading {
                        out.push_str(line);
                        out.push('\n');
                        let c = new_content.trim();
                        if !c.is_empty() {
                            out.push_str(c);
                            out.push('\n');
                        }
                    }
                    continue;
                }
            }
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !found {
        return Err(format!("heading not found: `{target}`"));
    }
    Ok(out.trim_end_matches('\n').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    fn mgr() -> (
        WorkItemManager<std::sync::Arc<dyn StorageBackend>>,
        std::sync::Arc<dyn StorageBackend>,
    ) {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (WorkItemManager::new(store.clone()), store)
    }

    fn engine(store: std::sync::Arc<dyn StorageBackend>) -> crate::SlcEngine {
        let llm: std::sync::Arc<dyn crate::LlmClient> =
            std::sync::Arc::new(crate::MockLlm::new(vec![]));
        crate::SlcEngine::with(store, llm, crate::SlcConfig::default())
    }

    /// Activation works for ANY document (skill, project, task) — the
    /// unified context anchor — and Skill is a first-class category.
    #[tokio::test]
    async fn document_activation_any_category_and_skill() {
        let (_, store) = mgr();
        let engine = engine(store);
        engine.seats.ensure_seat("seat_a").await.unwrap();

        // Skill is a document: parse + folder + RAG-eligible.
        assert_eq!(
            crate::model::DocumentCategory::parse("skill"),
            Some(crate::model::DocumentCategory::Skill)
        );
        assert_eq!(
            crate::model::DocumentCategory::parse("skills"),
            Some(crate::model::DocumentCategory::Skill)
        );
        assert!(crate::model::DocumentCategory::Skill.is_kb());
        let skill_doc = crate::model::Document::new(
            "skill_demo",
            crate::model::DocumentCategory::Skill,
            "demo",
            Default::default(),
            vec![],
            None,
        );
        assert_eq!(skill_doc.default_folder(), "docs/skills");

        // Activate a skill document.
        let mut skill = crate::model::Document::new(
            "skill_rust",
            crate::model::DocumentCategory::Skill,
            "Rust basics: borrow checker",
            Default::default(),
            vec![],
            Some("seat_a".into()),
        );
        engine.add_document(&mut skill).await.unwrap();
        assert!(
            engine
                .document_activate("seat_a", "skill_rust")
                .await
                .unwrap()
        );
        let active = engine.document_get_active("seat_a").await.unwrap().unwrap();
        assert_eq!(active.document_id, "skill_rust");
        assert_eq!(active.category, crate::model::DocumentCategory::Skill);

        // Activate a project — same effect.
        let mut proj = crate::model::Document::new(
            "project_vassista",
            crate::model::DocumentCategory::Project,
            "Vassista voice platform",
            Default::default(),
            vec![],
            Some("seat_a".into()),
        );
        engine.add_document(&mut proj).await.unwrap();
        assert!(
            engine
                .document_activate("seat_a", "project_vassista")
                .await
                .unwrap()
        );
        let active = engine.document_get_active("seat_a").await.unwrap().unwrap();
        assert_eq!(active.category, crate::model::DocumentCategory::Project);

        // Task activation keeps the unified pointer AND the legacy task one.
        let mut task = crate::model::Document::new(
            "task_x",
            crate::model::DocumentCategory::Task,
            "do things",
            Default::default(),
            vec![],
            Some("seat_a".into()),
        );
        engine.add_document(&mut task).await.unwrap();
        assert!(engine.document_activate("seat_a", "task_x").await.unwrap());
        let active = engine.document_get_active("seat_a").await.unwrap().unwrap();
        assert_eq!(active.category, crate::model::DocumentCategory::Task);
        let t = engine.task_get_active("seat_a").await.unwrap().unwrap();
        assert_eq!(t.task_id, "task_x");

        // Unknown document → false; deactivate clears both pointers.
        assert!(
            !engine
                .document_activate("seat_a", "missing_doc")
                .await
                .unwrap()
        );
        engine.document_deactivate("seat_a").await.unwrap();
        assert!(
            engine
                .document_get_active("seat_a")
                .await
                .unwrap()
                .is_none()
        );
        assert!(engine.task_get_active("seat_a").await.unwrap().is_none());
    }

    /// Legacy task activation (activate_task tool) also lands in the
    /// unified active-document pointer.
    #[tokio::test]
    async fn speaking_task_ids_with_collision_suffix() {
        let (m, _store) = mgr();
        let t1 = m
            .create_task("seat_a", "Миграция БД", "", None, &[], &json!({}))
            .await
            .unwrap();
        assert_eq!(t1.task_id, "migratsiya_bd");
        let t2 = m
            .create_task("seat_a", "Миграция БД", "", None, &[], &json!({}))
            .await
            .unwrap();
        assert_eq!(t2.task_id, "migratsiya_bd_2");
        let t3 = m
            .create_task("seat_a", "!!!", "", None, &[], &json!({}))
            .await
            .unwrap();
        assert!(t3.task_id.starts_with("task_")); // fallback: unique_id
        assert_ne!(t3.task_id, "task_");
    }

    #[tokio::test]
    async fn task_activation_sets_unified_pointer() {
        let (m, store) = mgr();
        let sm = crate::seat::SeatManager::new(store.clone(), 3600);
        sm.ensure_seat("seat_a").await.unwrap();
        let t = m
            .create_task("seat_a", "Task A", "", None, &[], &json!({}))
            .await
            .unwrap();
        assert!(m.set_active_task("seat_a", &t.task_id).await.unwrap());
        let engine = engine(store);
        let active = engine.document_get_active("seat_a").await.unwrap().unwrap();
        assert_eq!(active.document_id, t.task_id);

        assert!(
            !m.set_active_task("seat_a", "task_a_expanded")
                .await
                .unwrap()
        );
        let active = engine.document_get_active("seat_a").await.unwrap().unwrap();
        assert_eq!(active.document_id, t.task_id);
    }

    #[tokio::test]
    async fn task_crud_and_visibility() {
        let (m, _) = mgr();
        let t = m
            .create_task("seat_t", "Fix audio", "do it", None, &[], &json!({}))
            .await
            .unwrap();
        assert_eq!(t.task_id, "fix_audio"); // без категорийного префикса
        assert_eq!(t.status, STATUS_PENDING);

        let got = m.get_task("seat_t", &t.task_id).await.unwrap().unwrap();
        assert_eq!(got.name, "Fix audio");

        let upd = m
            .update_task(
                "seat_t",
                &t.task_id,
                Some("Fixed"),
                Some("done"),
                None,
                Some(None),
                None,
                Some(STATUS_COMPLETED),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(upd.status, STATUS_COMPLETED);
        assert_eq!(upd.description, "done");

        // other seat cannot see it
        assert!(m.get_task("other", &t.task_id).await.unwrap().is_none());

        let tasks = m
            .list_tasks("seat_t", None, Some(STATUS_COMPLETED), 10)
            .await
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(m.delete_task("seat_t", &t.task_id).await.unwrap());
        assert!(m.get_task("seat_t", &t.task_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn project_crud_and_task_link() {
        let (m, _) = mgr();
        let p = m
            .create_project(
                "seat_p",
                "Vassista",
                "voice assistant",
                &["core_manifest".into()],
                &json!({}),
            )
            .await
            .unwrap();
        assert_eq!(p.project_id, "vassista"); // без категорийного префикса
        assert_eq!(p.auto_load, vec!["core_manifest".to_string()]);

        let t = m
            .create_task(
                "seat_p",
                "STT",
                "stt plugin",
                Some(&p.project_id),
                &[],
                &json!({}),
            )
            .await
            .unwrap();
        assert_eq!(t.project_id.as_deref(), Some(p.project_id.as_str()));

        let projects = m.list_projects("seat_p", None, 10).await.unwrap();
        assert_eq!(projects.len(), 1);

        let upd = m
            .update_project(
                "seat_p",
                &p.project_id,
                None,
                None,
                None,
                None,
                Some(STATUS_PROJECT_ARCHIVED),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(upd.status, STATUS_PROJECT_ARCHIVED);
        assert!(m.delete_project("seat_p", &p.project_id).await.unwrap());
    }

    #[tokio::test]
    async fn active_task_pointer() {
        let (m, store) = mgr();
        let t = m
            .create_task("seat_a", "Task A", "", None, &[], &json!({}))
            .await
            .unwrap();
        // ensure the seat exists before setting the active pointer
        let sm = crate::seat::SeatManager::new(store, 3600);
        sm.ensure_seat("seat_a").await.unwrap();
        assert!(m.set_active_task("seat_a", &t.task_id).await.unwrap());
        let active = m.get_active_task("seat_a").await.unwrap().unwrap();
        assert_eq!(active.task_id, t.task_id);
        assert!(m.get_active_task("no_seat").await.unwrap().is_none());
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slug_name_transliterates_russian() {
        assert_eq!(
            crate::model::slug_name("Phase 8 M3: Mobile — ПОЛНЫЙ клиент"),
            "phase_8_m3_mobile_polnyy_klient"
        );
        assert_eq!(crate::model::slug_name("Миграция БД"), "migratsiya_bd");
        // Только мусор → пустой слаг (caller сделает unique_id fallback).
        assert_eq!(crate::model::slug_name("!!! ???"), "");
    }

    #[test]
    fn patch_append_prepend() {
        let out = apply_description_patch(
            "# План\n\nтело",
            &json!([{"op":"append","content":"## Итоги\nготово"},{"op":"prepend","content":"шапка"}]),
        )
        .unwrap();
        assert!(out.starts_with("шапка\n\n# План"));
        assert!(out.ends_with("## Итоги\nготово"));
    }

    #[test]
    fn patch_replace_and_remove_section() {
        let body = "# Задача\n\nвводная\n\n## Фаза 1\n\nстарый шаг\n\n### Подшаг\n\nдетали\n\n## Фаза 2\n\nфинал\n";
        // replace_section меняет только контент до следующего заголовка ТОГО ЖЕ
        // уровня (подшаги внутри — тоже затрагиваются).
        let out = apply_description_patch(
            body,
            &json!([{"op":"replace_section","heading":"## Фаза 1","content":"новый шаг"}]),
        )
        .unwrap();
        assert!(out.contains("## Фаза 1\nновый шаг"));
        assert!(!out.contains("старый шаг"));
        assert!(!out.contains("Подшаг"));
        assert!(out.contains("## Фаза 2\n\nфинал"));
        // remove_section убирает и заголовок.
        let out2 = apply_description_patch(
            body,
            &json!([{"op":"remove_section","heading":"## Фаза 2"}]),
        )
        .unwrap();
        assert!(!out2.contains("Фаза 2"));
        assert!(!out2.contains("финал"));
        assert!(out2.contains("## Фаза 1"));
    }

    #[test]
    fn patch_errors_are_explicit() {
        let e = apply_description_patch("x", &json!("not array")).unwrap_err();
        assert!(e.contains("array"));
        let e = apply_description_patch("x", &json!([{"op":"noop"}])).unwrap_err();
        assert!(e.contains("unknown patch op"));
        let e = apply_description_patch(
            "# A\n\nтекст",
            &json!([{"op":"replace_section","heading":"## Нет такого","content":"y"}]),
        )
        .unwrap_err();
        assert!(e.contains("not found"));
    }
}
