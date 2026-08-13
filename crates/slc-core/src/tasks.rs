//! Tasks and projects — modeled as unified `Document`s (category `Task` /
//! `Project`), per the user-approved design rule: "проекты, задачи и
//! документы — всё это документы". Task/project-specific state (status,
//! project link, auto_load) lives in `Document::metadata.extra` and the
//! typed `auto_load` field.
//!
//! Legacy equivalent: the Mongo `tasks` / `projects` collections behind
//! `src/slc_mcp/tools/http/tasks.py` and `projects.py`.

use crate::error::SlcResult;
use crate::model::{DocMeta, Document, DocumentCategory, content_hash};
use crate::storage::{DocFilter, DocSort, SortDir, StorageBackend};
use chrono::Utc;
use serde_json::{json, Value};

/// Status values shared by tasks and projects.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_CANCELLED: &str = "cancelled";
pub const STATUS_ARCHIVED: &str = "archived";

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
    doc.metadata.extra.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn doc_to_task(doc: &Document) -> TaskInfo {
    TaskInfo {
        task_id: doc.document_id.clone(),
        name: extra_field(doc, "name").unwrap_or_else(|| doc.document_id.clone()),
        description: doc.content.clone(),
        status: extra_field(doc, "status").unwrap_or_else(|| STATUS_PENDING.into()),
        project_id: extra_field(doc, "project_id"),
        auto_load: doc.auto_load.clone(),
        created_at: doc.created_at.to_rfc3339(),
        updated_at: doc.updated_at.to_rfc3339(),
    }
}

fn doc_to_project(doc: &Document) -> ProjectInfo {
    ProjectInfo {
        project_id: doc.document_id.clone(),
        name: extra_field(doc, "name").unwrap_or_else(|| doc.document_id.clone()),
        description: doc.content.clone(),
        status: extra_field(doc, "status").unwrap_or_else(|| STATUS_ACTIVE.into()),
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
        meta.extra.insert("project_id".into(), json!(pid));
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
        let task_id = crate::model::unique_id("task");
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

    pub async fn get_task(&self, seat_id: &str, task_id: &str) -> SlcResult<Option<TaskInfo>> {
        let Some(doc) = self.store.kb_get(task_id).await? else { return Ok(None) };
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
        project_id: Option<Option<&str>>,
        auto_load: Option<&[String]>,
        status: Option<&str>,
        metadata: Option<&Value>,
    ) -> SlcResult<Option<TaskInfo>> {
        let Some(mut doc) = self.store.kb_get(task_id).await? else { return Ok(None) };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Task {
            return Ok(None);
        }
        if let Some(n) = name {
            doc.metadata.extra.insert("name".into(), json!(n));
        }
        if let Some(d) = description {
            doc.content = d.into();
            doc.content_hash = content_hash(&doc.content);
        }
        if let Some(Some(pid)) = project_id {
            doc.metadata.extra.insert("project_id".into(), json!(pid));
        }
        if let Some(pid) = project_id {
            if pid.is_none() {
                doc.metadata.extra.remove("project_id");
            }
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
        Ok(Some(doc_to_task(&doc)))
    }

    pub async fn delete_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let Some(doc) = self.store.kb_get(task_id).await? else { return Ok(false) };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Task {
            return Ok(false);
        }
        self.store.kb_purge(task_id).await
    }

    pub async fn list_tasks(&self, seat_id: &str, project_id: Option<&str>, status: Option<&str>, limit: usize) -> SlcResult<Vec<TaskInfo>> {
        let filter = DocFilter {
            category: Some(DocumentCategory::Task),
            seat_id: Some(seat_id.into()),
            ..Default::default()
        };
        let docs = self.store.kb_find(&filter, &DocSort::by_updated(SortDir::Desc), 500).await?;
        let mut out: Vec<TaskInfo> = docs
            .into_iter()
            .filter(|d| d.is_kb_visible(seat_id))
            .map(|d| doc_to_task(&d))
            .filter(|t| project_id.map(|p| t.project_id.as_deref() == Some(p)).unwrap_or(true))
            .filter(|t| status.map(|s| t.status == s).unwrap_or(true))
            .collect();
        out.truncate(limit);
        Ok(out)
    }

    /// Set the seat's active task pointer (working-memory context).
    pub async fn set_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        self.store
            .set_seat_active_task(seat_id, task_id)
            .await
    }

    pub async fn get_active_task(&self, seat_id: &str) -> SlcResult<Option<TaskInfo>> {
        let Some(seat) = self.store.get_seat(seat_id).await? else { return Ok(None) };
        let Some(task_id) = seat.active_task_id else { return Ok(None) };
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
        let project_id = crate::model::unique_id("project");
        let doc = build_doc(
            project_id.clone(),
            DocumentCategory::Project,
            name,
            description,
            STATUS_ACTIVE,
            None,
            auto_load,
            metadata,
            seat_id,
        );
        self.store.kb_insert(&doc).await?;
        Ok(doc_to_project(&doc))
    }

    pub async fn get_project(&self, seat_id: &str, project_id: &str) -> SlcResult<Option<ProjectInfo>> {
        let Some(doc) = self.store.kb_get(project_id).await? else { return Ok(None) };
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
        auto_load: Option<&[String]>,
        status: Option<&str>,
        metadata: Option<&Value>,
    ) -> SlcResult<Option<ProjectInfo>> {
        let Some(mut doc) = self.store.kb_get(project_id).await? else { return Ok(None) };
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
        let Some(doc) = self.store.kb_get(project_id).await? else { return Ok(false) };
        if !doc.is_kb_visible(seat_id) || doc.category != DocumentCategory::Project {
            return Ok(false);
        }
        self.store.kb_purge(project_id).await
    }

    pub async fn list_projects(&self, seat_id: &str, status: Option<&str>, limit: usize) -> SlcResult<Vec<ProjectInfo>> {
        let filter = DocFilter {
            category: Some(DocumentCategory::Project),
            seat_id: Some(seat_id.into()),
            ..Default::default()
        };
        let docs = self.store.kb_find(&filter, &DocSort::by_updated(SortDir::Desc), 500).await?;
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
    fn is_kb_visible(&self, seat_id: &str) -> bool {
        match &self.seat_id {
            None => true,
            Some(owner) => owner == seat_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    fn mgr() -> (WorkItemManager<std::sync::Arc<dyn StorageBackend>>, std::sync::Arc<dyn StorageBackend>) {
        let store: std::sync::Arc<dyn StorageBackend> = std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (WorkItemManager::new(store.clone()), store)
    }

    #[tokio::test]
    async fn task_crud_and_visibility() {
        let (m, _) = mgr();
        let t = m.create_task("seat_t", "Fix audio", "do it", None, &[], &json!({})).await.unwrap();
        assert!(t.task_id.starts_with("task_"));
        assert_eq!(t.status, STATUS_PENDING);

        let got = m.get_task("seat_t", &t.task_id).await.unwrap().unwrap();
        assert_eq!(got.name, "Fix audio");

        let upd = m.update_task("seat_t", &t.task_id, Some("Fixed"), Some("done"), Some(None), None, Some(STATUS_COMPLETED), None).await.unwrap().unwrap();
        assert_eq!(upd.status, STATUS_COMPLETED);
        assert_eq!(upd.description, "done");

        // other seat cannot see it
        assert!(m.get_task("other", &t.task_id).await.unwrap().is_none());

        let tasks = m.list_tasks("seat_t", None, Some(STATUS_COMPLETED), 10).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(m.delete_task("seat_t", &t.task_id).await.unwrap());
        assert!(m.get_task("seat_t", &t.task_id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn project_crud_and_task_link() {
        let (m, _) = mgr();
        let p = m.create_project("seat_p", "Vassista", "voice assistant", &["core_manifest".into()], &json!({})).await.unwrap();
        assert!(p.project_id.starts_with("project_"));
        assert_eq!(p.auto_load, vec!["core_manifest".to_string()]);

        let t = m.create_task("seat_p", "STT", "stt plugin", Some(&p.project_id), &[], &json!({})).await.unwrap();
        assert_eq!(t.project_id.as_deref(), Some(p.project_id.as_str()));

        let projects = m.list_projects("seat_p", None, 10).await.unwrap();
        assert_eq!(projects.len(), 1);

        let upd = m.update_project("seat_p", &p.project_id, None, None, None, Some(STATUS_ARCHIVED), None).await.unwrap().unwrap();
        assert_eq!(upd.status, STATUS_ARCHIVED);
        assert!(m.delete_project("seat_p", &p.project_id).await.unwrap());
    }

    #[tokio::test]
    async fn active_task_pointer() {
        let (m, store) = mgr();
        let t = m.create_task("seat_a", "Task A", "", None, &[], &json!({})).await.unwrap();
        // ensure the seat exists before setting the active pointer
        let sm = crate::seat::SeatManager::new(store, 3600);
        sm.ensure_seat("seat_a").await.unwrap();
        assert!(m.set_active_task("seat_a", &t.task_id).await.unwrap());
        let active = m.get_active_task("seat_a").await.unwrap().unwrap();
        assert_eq!(active.task_id, t.task_id);
        assert!(m.get_active_task("no_seat").await.unwrap().is_none());
    }
}
