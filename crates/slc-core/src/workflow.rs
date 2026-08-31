//! Transport-neutral task workflow.
//!
//! SLC owns task identity, assignment, lineage, status, reports, and the
//! immutable per-task event stream.  A delivery adapter (today Swarm MCP,
//! later Matrix or another bus) may wake a participant, but it never decides
//! whether a task exists, who issued it, or whether it is complete.

use crate::SlcEngine;
use crate::error::{SlcError, SlcResult};
use crate::model::{Document, DocumentCategory, content_hash};
use crate::storage::{DocFilter, DocSort, SortDir};
use crate::tasks::{
    STATUS_ACTIVE, STATUS_BLOCKED, STATUS_COMPLETED, STATUS_FAILED, TaskInfo, doc_to_task,
    normalize_task_status,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

const TASK_EVENTS: &str = "task_events_v1";
const TASK_EVENT_IDEMPOTENCY: &str = "task_event_idempotency_v1";
const TASK_ASSIGN_IDEMPOTENCY: &str = "task_assign_idempotency_v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEventKind {
    Created,
    Started,
    Progress,
    Message,
    Report,
    StatusChanged,
    Transport,
}

impl TaskEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Started => "started",
            Self::Progress => "progress",
            Self::Message => "message",
            Self::Report => "report",
            Self::StatusChanged => "status_changed",
            Self::Transport => "transport",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskListScope {
    Visible,
    Assigned,
    Issued,
}

impl TaskListScope {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "visible" | "all" => Some(Self::Visible),
            "assigned" | "assigned_to_me" | "mine" => Some(Self::Assigned),
            "issued" | "issued_by_me" | "delegated" => Some(Self::Issued),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub event_id: String,
    pub task_id: String,
    pub kind: TaskEventKind,
    pub actor: String,
    pub recipient: Option<String>,
    pub status: Option<String>,
    pub message: String,
    pub metadata: Value,
    pub created_at: String,
}

fn workflow_string(doc: &Document, key: &str) -> Option<String> {
    doc.metadata
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(String::from)
}

fn task_document(doc: Option<Document>, task_id: &str) -> SlcResult<Document> {
    match doc {
        Some(doc) if doc.category == DocumentCategory::Task => Ok(doc),
        _ => Err(SlcError::NotFound(task_id.to_string())),
    }
}

fn clean_required(value: &str, field: &str, max_chars: usize) -> SlcResult<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(SlcError::InvalidInput(format!("{field} must not be empty")));
    }
    if value.chars().count() > max_chars {
        return Err(SlcError::Limit(format!(
            "{field} exceeds {max_chars} characters"
        )));
    }
    Ok(value.to_string())
}

fn idempotency_key(actor: &str, supplied: &str) -> String {
    content_hash(&format!("{actor}\0{supplied}"))
}

#[allow(clippy::too_many_arguments)]
fn task_event_fingerprint(
    task_id: &str,
    kind: TaskEventKind,
    actor: &str,
    recipient: Option<&str>,
    status: Option<&str>,
    message: &str,
    metadata: &Value,
) -> SlcResult<String> {
    Ok(content_hash(&serde_json::to_string(&json!({
        "task_id": task_id,
        "kind": kind,
        "actor": actor,
        "recipient": recipient,
        "status": status,
        "message": message,
        "metadata": metadata,
    }))?))
}

fn visual_claim(message: &str) -> Option<&'static str> {
    const CLAIMS: &[&str] = &[
        "visual pass",
        "visual qa pass",
        "visually verified",
        "visual match",
        "looks correct",
        "matches the design",
        "matched the design",
        "design masters matched",
        "pixel-perfect",
        "visually correct",
        "визуально проверено",
        "визуальная проверка пройдена",
        "визуально соответствует",
        "соответствует макету",
        "совпадает с макетом",
    ];
    let normalized = message
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    CLAIMS
        .iter()
        .copied()
        .find(|claim| normalized.contains(claim))
}

impl SlcEngine {
    async fn existing_task_event(
        &self,
        actor: &str,
        supplied_idempotency_key: Option<&str>,
        fingerprint: &str,
    ) -> SlcResult<Option<TaskEvent>> {
        let Some(supplied) = supplied_idempotency_key else {
            return Ok(None);
        };
        let supplied = clean_required(supplied, "idempotency_key", 200)?;
        let key = idempotency_key(actor, &supplied);
        let Some(existing) = self
            .store()
            .get_record(TASK_EVENT_IDEMPOTENCY, &key)
            .await?
        else {
            return Ok(None);
        };
        if existing.get("fingerprint").and_then(Value::as_str) != Some(fingerprint) {
            return Err(SlcError::InvalidInput(
                "idempotency_key was already used for a different task event".into(),
            ));
        }
        let event_id = existing
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| SlcError::Storage("task event idempotency record is corrupt".into()))?;
        let event = self
            .store()
            .get_record(TASK_EVENTS, event_id)
            .await?
            .ok_or_else(|| SlcError::Storage("idempotent task event is missing".into()))?;
        Ok(Some(serde_json::from_value(event)?))
    }

    /// Stable workflow principal for a seat. Unregistered deployments keep
    /// the historical one-principal-per-seat behaviour.
    pub fn workflow_principal(&self, seat_id: &str) -> String {
        self.config
            .principal_seats
            .iter()
            .find_map(|(principal, seat)| (seat == seat_id).then(|| principal.clone()))
            .unwrap_or_else(|| seat_id.to_string())
    }

    pub fn workflow_seat(&self, principal: &str) -> Option<String> {
        self.config
            .principal_seats
            .get(principal)
            .cloned()
            .or_else(|| {
                self.config
                    .principal_seats
                    .is_empty()
                    .then(|| principal.to_string())
            })
    }

    pub fn workflow_assignment_targets(&self, seat_id: &str) -> Vec<String> {
        let principal = self.workflow_principal(seat_id);
        let mut targets = self
            .config
            .task_assign_acl
            .get(&principal)
            .map(|targets| targets.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if !targets.iter().any(|target| target == &principal) {
            targets.push(principal);
        }
        targets.sort();
        targets
    }

    pub fn can_assign_task(&self, seat_id: &str, assignee: &str) -> bool {
        let actor = self.workflow_principal(seat_id);
        actor == assignee
            || self
                .config
                .task_assign_acl
                .get(&actor)
                .is_some_and(|targets| targets.contains(assignee) || targets.contains("*"))
    }

    fn task_access_allowed(&self, seat_id: &str, doc: &Document) -> bool {
        let principal = self.workflow_principal(seat_id);
        let is_participant = workflow_string(doc, "issuer").as_deref() == Some(&principal)
            || workflow_string(doc, "assignee").as_deref() == Some(&principal);
        let owns_document = doc.seat_id.as_deref() == Some(seat_id);
        let global_coordinator = self
            .config
            .task_assign_acl
            .get(&principal)
            .is_some_and(|targets| targets.contains("*"));
        let scoped_operator = doc
            .seat_id
            .as_deref()
            .is_some_and(|owner| self.can_manage_target(seat_id, owner));
        owns_document || is_participant || global_coordinator || scoped_operator
    }

    async fn workflow_task_document(&self, seat_id: &str, task_id: &str) -> SlcResult<Document> {
        let doc = task_document(self.store().kb_get(task_id).await?, task_id)?;
        if !self.task_access_allowed(seat_id, &doc) {
            return Err(SlcError::PermissionDenied(format!(
                "principal {} cannot access task {task_id}",
                self.workflow_principal(seat_id)
            )));
        }
        Ok(doc)
    }

    async fn append_task_event(
        &self,
        task_id: &str,
        kind: TaskEventKind,
        actor: &str,
        recipient: Option<&str>,
        status: Option<&str>,
        message: &str,
        metadata: Value,
        supplied_idempotency_key: Option<&str>,
    ) -> SlcResult<TaskEvent> {
        let fingerprint =
            task_event_fingerprint(task_id, kind, actor, recipient, status, message, &metadata)?;
        if let Some(event) = self
            .existing_task_event(actor, supplied_idempotency_key, &fingerprint)
            .await?
        {
            return Ok(event);
        }

        let now = Utc::now();
        let event = TaskEvent {
            event_id: format!("task_event_{}", Uuid::new_v4().simple()),
            task_id: task_id.to_string(),
            kind,
            actor: actor.to_string(),
            recipient: recipient.map(String::from),
            status: status.map(String::from),
            message: message.to_string(),
            metadata,
            created_at: now.to_rfc3339(),
        };
        self.store()
            .put_record(TASK_EVENTS, &event.event_id, &serde_json::to_value(&event)?)
            .await?;
        if let Some(supplied) = supplied_idempotency_key {
            let key = idempotency_key(actor, supplied.trim());
            self.store()
                .put_record(
                    TASK_EVENT_IDEMPOTENCY,
                    &key,
                    &json!({"fingerprint": fingerprint, "event_id": event.event_id}),
                )
                .await?;
        }
        Ok(event)
    }

    async fn touch_task_projection(
        &self,
        task_id: &str,
        event: &TaskEvent,
        status: Option<&str>,
        terminal_summary: Option<&str>,
    ) -> SlcResult<TaskInfo> {
        let mut doc = task_document(self.store().kb_get(task_id).await?, task_id)?;
        doc.metadata
            .extra
            .insert("last_event_id".into(), json!(event.event_id));
        doc.metadata
            .extra
            .insert("last_event_at".into(), json!(event.created_at));
        if let Some(status) = status {
            doc.metadata.extra.insert("status".into(), json!(status));
            if matches!(status, STATUS_COMPLETED | STATUS_BLOCKED | STATUS_FAILED) {
                doc.metadata
                    .extra
                    .insert("terminal_at".into(), json!(event.created_at));
            }
        }
        if let Some(summary) = terminal_summary {
            doc.metadata
                .extra
                .insert("terminal_summary".into(), json!(summary));
        }
        doc.updated_at = Utc::now();
        doc.version += 1;
        self.store().kb_replace(&doc).await?;
        Ok(doc_to_task(&doc))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn workflow_assign_task(
        &self,
        seat_id: &str,
        assignee: &str,
        name: &str,
        description: &str,
        parent_task_id: Option<&str>,
        project_id: Option<&str>,
        auto_load: &[String],
        metadata: &Value,
        supplied_idempotency_key: Option<&str>,
    ) -> SlcResult<TaskInfo> {
        let _workflow_guard = self.workflow_lock.lock().await;
        let actor = self.workflow_principal(seat_id);
        let assignee = clean_required(assignee, "assignee", 128)?;
        if !self.can_assign_task(seat_id, &assignee) {
            return Err(SlcError::PermissionDenied(format!(
                "principal {actor} cannot assign tasks to {assignee}"
            )));
        }
        let target_seat = self.workflow_seat(&assignee).ok_or_else(|| {
            SlcError::InvalidInput(format!("unknown workflow principal: {assignee}"))
        })?;
        self.seats.ensure_seat(&target_seat).await?;

        let parent = if let Some(parent_id) = parent_task_id {
            Some(self.workflow_task_document(seat_id, parent_id).await?)
        } else {
            None
        };
        let root_task_id = parent.as_ref().map(|doc| {
            workflow_string(doc, "root_task_id").unwrap_or_else(|| doc.document_id.clone())
        });

        let fingerprint = content_hash(&serde_json::to_string(&json!({
            "issuer": actor,
            "assignee": assignee,
            "name": name,
            "description": description,
            "parent_task_id": parent_task_id,
            "project_id": project_id,
            "auto_load": auto_load,
            "metadata": metadata,
        }))?);
        if let Some(supplied) = supplied_idempotency_key {
            let supplied = clean_required(supplied, "idempotency_key", 200)?;
            let key = idempotency_key(&actor, &supplied);
            if let Some(existing) = self
                .store()
                .get_record(TASK_ASSIGN_IDEMPOTENCY, &key)
                .await?
            {
                if existing.get("fingerprint").and_then(Value::as_str) != Some(&fingerprint) {
                    return Err(SlcError::InvalidInput(
                        "idempotency_key was already used for a different task assignment".into(),
                    ));
                }
                let task_id = existing
                    .get("task_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        SlcError::Storage("task assignment idempotency record is corrupt".into())
                    })?;
                return self.workflow_get_task(seat_id, task_id).await;
            }
        }

        let caller_metadata = metadata
            .as_object()
            .cloned()
            .ok_or_else(|| SlcError::InvalidInput("metadata must be a JSON object".into()))?;
        let mut workflow_metadata = Map::new();
        workflow_metadata.insert("workflow_metadata".into(), Value::Object(caller_metadata));
        workflow_metadata.insert("workflow_version".into(), json!(1));
        workflow_metadata.insert("issuer".into(), json!(actor));
        workflow_metadata.insert("assignee".into(), json!(assignee));
        workflow_metadata.insert("assigned_at".into(), json!(Utc::now().to_rfc3339()));
        if let Some(parent_id) = parent_task_id {
            workflow_metadata.insert("parent_task_id".into(), json!(parent_id));
        }
        if let Some(root_id) = root_task_id.as_deref() {
            workflow_metadata.insert("root_task_id".into(), json!(root_id));
        }

        let task = self
            .task_create(
                &target_seat,
                &clean_required(name, "name", 500)?,
                description,
                project_id,
                auto_load,
                &Value::Object(workflow_metadata),
            )
            .await?;
        let event = self
            .append_task_event(
                &task.task_id,
                TaskEventKind::Created,
                &actor,
                Some(&assignee),
                Some(crate::tasks::STATUS_PENDING),
                "Task assigned",
                json!({"parent_task_id": parent_task_id}),
                None,
            )
            .await?;
        let task = self
            .touch_task_projection(&task.task_id, &event, None, None)
            .await?;
        if let Some(supplied) = supplied_idempotency_key {
            let key = idempotency_key(&actor, supplied.trim());
            self.store()
                .put_record(
                    TASK_ASSIGN_IDEMPOTENCY,
                    &key,
                    &json!({"fingerprint": fingerprint, "task_id": task.task_id}),
                )
                .await?;
        }
        Ok(task)
    }

    pub async fn workflow_get_task(&self, seat_id: &str, task_id: &str) -> SlcResult<TaskInfo> {
        let doc = self.workflow_task_document(seat_id, task_id).await?;
        Ok(doc_to_task(&doc))
    }

    pub async fn workflow_list_tasks(
        &self,
        seat_id: &str,
        scope: TaskListScope,
        status: Option<&str>,
        project_id: Option<&str>,
        assignee: Option<&str>,
        issuer: Option<&str>,
        limit: usize,
    ) -> SlcResult<Vec<TaskInfo>> {
        let principal = self.workflow_principal(seat_id);
        let normalized_status = match status {
            Some(raw) => Some(normalize_task_status(raw).ok_or_else(|| {
                SlcError::InvalidInput(format!("unsupported task status: {raw}"))
            })?),
            None => None,
        };
        let docs = self
            .store()
            .kb_find(
                &DocFilter {
                    category: Some(DocumentCategory::Task),
                    ..Default::default()
                },
                &DocSort::by_updated(SortDir::Desc),
                5_000,
            )
            .await?;
        let mut tasks = docs
            .into_iter()
            .filter(|doc| self.task_access_allowed(seat_id, doc))
            .map(|doc| doc_to_task(&doc))
            .filter(|task| match scope {
                TaskListScope::Visible => true,
                TaskListScope::Assigned => task.assignee.as_deref() == Some(&principal),
                TaskListScope::Issued => task.issuer.as_deref() == Some(&principal),
            })
            .filter(|task| normalized_status.is_none_or(|value| task.status == value))
            .filter(|task| project_id.is_none_or(|value| task.project_id.as_deref() == Some(value)))
            .filter(|task| assignee.is_none_or(|value| task.assignee.as_deref() == Some(value)))
            .filter(|task| issuer.is_none_or(|value| task.issuer.as_deref() == Some(value)))
            .take(limit.min(500))
            .collect::<Vec<_>>();
        tasks.truncate(limit.min(500));
        Ok(tasks)
    }

    pub async fn workflow_start_task(
        &self,
        seat_id: &str,
        task_id: &str,
        message: &str,
        idempotency_key: Option<&str>,
    ) -> SlcResult<(TaskInfo, TaskEvent)> {
        let _workflow_guard = self.workflow_lock.lock().await;
        let doc = self.workflow_task_document(seat_id, task_id).await?;
        let actor = self.workflow_principal(seat_id);
        if workflow_string(&doc, "assignee").as_deref() != Some(&actor) {
            return Err(SlcError::PermissionDenied(
                "only the assignee can start a task".into(),
            ));
        }
        let current_status = workflow_string(&doc, "status")
            .unwrap_or_else(|| crate::tasks::STATUS_PENDING.to_string());
        if matches!(
            current_status.as_str(),
            STATUS_COMPLETED | STATUS_BLOCKED | STATUS_FAILED | crate::tasks::STATUS_CANCELLED
        ) {
            return Err(SlcError::InvalidInput(format!(
                "terminal task cannot be started again: {current_status}"
            )));
        }
        let event = self
            .append_task_event(
                task_id,
                TaskEventKind::Started,
                &actor,
                workflow_string(&doc, "issuer").as_deref(),
                Some(STATUS_ACTIVE),
                message.trim(),
                json!({}),
                idempotency_key,
            )
            .await?;
        let task = self
            .touch_task_projection(task_id, &event, Some(STATUS_ACTIVE), None)
            .await?;
        let assignee_seat = self.workflow_seat(&actor).ok_or_else(|| {
            SlcError::InvalidInput(format!("unknown workflow principal: {actor}"))
        })?;
        self.task_activate(&assignee_seat, task_id).await?;
        Ok((task, event))
    }

    pub async fn workflow_report_task(
        &self,
        seat_id: &str,
        task_id: &str,
        raw_status: &str,
        summary: &str,
        metadata: Value,
        idempotency_key: Option<&str>,
    ) -> SlcResult<(TaskInfo, TaskEvent)> {
        let _workflow_guard = self.workflow_lock.lock().await;
        let doc = self.workflow_task_document(seat_id, task_id).await?;
        let actor = self.workflow_principal(seat_id);
        let assignee = workflow_string(&doc, "assignee")
            .unwrap_or_else(|| doc.seat_id.clone().unwrap_or_default());
        let global_coordinator = self
            .config
            .task_assign_acl
            .get(&actor)
            .is_some_and(|targets| targets.contains("*"));
        if actor != assignee && !global_coordinator {
            return Err(SlcError::PermissionDenied(
                "only the assignee or a global task coordinator can report this task".into(),
            ));
        }
        let status = normalize_task_status(raw_status).ok_or_else(|| {
            SlcError::InvalidInput(
                "status must be in_progress, completed, blocked, or failed".into(),
            )
        })?;
        if !matches!(
            status,
            STATUS_ACTIVE | STATUS_COMPLETED | STATUS_BLOCKED | STATUS_FAILED
        ) {
            return Err(SlcError::InvalidInput(
                "status must be in_progress, completed, blocked, or failed".into(),
            ));
        }
        let summary = clean_required(summary, "summary", 100_000)?;
        let terminal = matches!(status, STATUS_COMPLETED | STATUS_BLOCKED | STATUS_FAILED);
        let event_kind = if terminal {
            TaskEventKind::Report
        } else {
            TaskEventKind::Progress
        };
        let issuer = workflow_string(&doc, "issuer");
        let current_status = workflow_string(&doc, "status")
            .unwrap_or_else(|| crate::tasks::STATUS_PENDING.to_string());
        if matches!(
            current_status.as_str(),
            STATUS_COMPLETED | STATUS_BLOCKED | STATUS_FAILED | crate::tasks::STATUS_CANCELLED
        ) {
            if current_status != status
                || workflow_string(&doc, "terminal_summary").as_deref() != Some(summary.as_str())
            {
                return Err(SlcError::InvalidInput(format!(
                    "terminal task cannot transition from {current_status} to {status}"
                )));
            }
            let fingerprint = task_event_fingerprint(
                task_id,
                event_kind,
                &actor,
                issuer.as_deref(),
                Some(status),
                &summary,
                &metadata,
            )?;
            if let Some(event) = self
                .existing_task_event(&actor, idempotency_key, &fingerprint)
                .await?
            {
                return Ok((doc_to_task(&doc), event));
            }
            return Err(SlcError::InvalidInput(
                "terminal task already has a report; retry only with the original idempotency_key"
                    .into(),
            ));
        }
        if status == STATUS_COMPLETED
            && self.config.text_only_principals.contains(&actor)
            && let Some(claim) = visual_claim(&summary)
        {
            return Err(SlcError::InvalidInput(format!(
                "text-only principal cannot assert visual completion ({claim}); report numeric evidence and request vision-capable verification"
            )));
        }
        let event = self
            .append_task_event(
                task_id,
                event_kind,
                &actor,
                issuer.as_deref(),
                Some(status),
                &summary,
                metadata,
                idempotency_key,
            )
            .await?;
        let task = self
            .touch_task_projection(
                task_id,
                &event,
                Some(status),
                terminal.then_some(summary.as_str()),
            )
            .await?;
        if terminal
            && let Some(assignee_seat) = self.workflow_seat(&assignee)
            && let Some(seat) = self.seats.get_seat(&assignee_seat).await?
            && seat.active_task_id.as_deref() == Some(task_id)
        {
            self.seats
                .set_active_task(&assignee_seat, None, None)
                .await?;
        }
        Ok((task, event))
    }

    pub async fn workflow_task_message(
        &self,
        seat_id: &str,
        task_id: &str,
        recipient: &str,
        message: &str,
        metadata: Value,
        idempotency_key: Option<&str>,
    ) -> SlcResult<TaskEvent> {
        let _workflow_guard = self.workflow_lock.lock().await;
        let doc = self.workflow_task_document(seat_id, task_id).await?;
        let actor = self.workflow_principal(seat_id);
        let recipient = clean_required(recipient, "recipient", 128)?;
        let issuer = workflow_string(&doc, "issuer");
        let assignee = workflow_string(&doc, "assignee");
        if issuer.as_deref() != Some(&recipient) && assignee.as_deref() != Some(&recipient) {
            return Err(SlcError::PermissionDenied(
                "task messages may target only the issuer or assignee".into(),
            ));
        }
        let message = clean_required(message, "message", 100_000)?;
        let event = self
            .append_task_event(
                task_id,
                TaskEventKind::Message,
                &actor,
                Some(&recipient),
                None,
                &message,
                metadata,
                idempotency_key,
            )
            .await?;
        self.touch_task_projection(task_id, &event, None, None)
            .await?;
        Ok(event)
    }

    pub async fn workflow_task_events(
        &self,
        seat_id: &str,
        task_id: &str,
        limit: usize,
    ) -> SlcResult<Vec<TaskEvent>> {
        self.workflow_task_document(seat_id, task_id).await?;
        let mut events = self
            .store()
            .list_records(TASK_EVENTS)
            .await?
            .into_iter()
            .filter_map(|(_, value)| serde_json::from_value::<TaskEvent>(value).ok())
            .filter(|event| event.task_id == task_id)
            .collect::<Vec<_>>();
        events.sort_by(|left, right| left.created_at.cmp(&right.created_at));
        if events.len() > limit.min(500) {
            events.drain(0..events.len() - limit.min(500));
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;
    use crate::{MockLlm, SlcConfig, StorageBackend};
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    fn engine() -> SlcEngine {
        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let config = SlcConfig {
            principal_seats: HashMap::from([
                ("manager".into(), "seat-manager".into()),
                ("senior".into(), "seat-senior".into()),
                ("junior".into(), "seat-junior".into()),
            ]),
            task_assign_acl: HashMap::from([
                ("manager".into(), HashSet::from(["*".into()])),
                ("senior".into(), HashSet::from(["junior".into()])),
            ]),
            text_only_principals: HashSet::from(["junior".into()]),
            ..Default::default()
        };
        SlcEngine::with(store, Arc::new(MockLlm::new(vec![])), config)
    }

    #[tokio::test]
    async fn assignment_lineage_reports_and_messages_live_in_slc() {
        let engine = engine();
        engine.seats.ensure_seat("seat-senior").await.unwrap();
        let parent = engine
            .workflow_assign_task(
                "seat-manager",
                "senior",
                "Build site",
                "Own delivery",
                None,
                None,
                &[],
                &json!({}),
                Some("parent-1"),
            )
            .await
            .unwrap();
        let child = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Patch hero copy",
                "One file only",
                Some(&parent.task_id),
                None,
                &[],
                &json!({"expected":"exact text"}),
                Some("child-1"),
            )
            .await
            .unwrap();
        assert_eq!(child.issuer.as_deref(), Some("senior"));
        assert_eq!(child.assignee.as_deref(), Some("junior"));
        assert_eq!(
            child.parent_task_id.as_deref(),
            Some(parent.task_id.as_str())
        );
        assert_eq!(child.root_task_id.as_deref(), Some(parent.task_id.as_str()));
        assert!(
            engine
                .task_get_active("seat-junior")
                .await
                .unwrap()
                .is_none(),
            "assignment queues work but must not steal the assignee's active context"
        );

        let (started, _) = engine
            .workflow_start_task("seat-junior", &child.task_id, "Starting", Some("start-1"))
            .await
            .unwrap();
        assert_eq!(started.status, STATUS_ACTIVE);
        assert_eq!(
            engine
                .task_get_active("seat-junior")
                .await
                .unwrap()
                .unwrap()
                .task_id,
            child.task_id
        );
        engine
            .workflow_task_message(
                "seat-junior",
                &child.task_id,
                "senior",
                "Need exact mobile width",
                json!({}),
                Some("message-1"),
            )
            .await
            .unwrap();
        let (reported, _) = engine
            .workflow_report_task(
                "seat-junior",
                &child.task_id,
                "completed",
                "Text replaced; checksum recorded; visual review pending senior",
                json!({"sha256":"abc"}),
                Some("report-1"),
            )
            .await
            .unwrap();
        assert_eq!(reported.status, STATUS_COMPLETED);
        assert!(
            engine
                .task_get_active("seat-junior")
                .await
                .unwrap()
                .is_none(),
            "a terminal report must release the assignee's active task anchor"
        );
        let events = engine
            .workflow_task_events("seat-senior", &child.task_id, 20)
            .await
            .unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events.last().unwrap().kind, TaskEventKind::Report);
        let issued = engine
            .workflow_list_tasks(
                "seat-senior",
                TaskListScope::Issued,
                None,
                None,
                None,
                None,
                20,
            )
            .await
            .unwrap();
        assert_eq!(issued.len(), 1);
    }

    #[tokio::test]
    async fn idempotency_and_text_only_visual_policy_are_enforced() {
        let engine = engine();
        let first = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Narrow patch",
                "Edit one selector",
                None,
                None,
                &[],
                &json!({}),
                Some("same-assignment"),
            )
            .await
            .unwrap();
        let replay = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Narrow patch",
                "Edit one selector",
                None,
                None,
                &[],
                &json!({}),
                Some("same-assignment"),
            )
            .await
            .unwrap();
        assert_eq!(first.task_id, replay.task_id);
        let error = engine
            .workflow_report_task(
                "seat-junior",
                &first.task_id,
                "completed",
                "Visual QA PASS; matches the design",
                json!({}),
                Some("bad-report"),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("text-only principal"));

        let error = engine
            .workflow_report_task(
                "seat-junior",
                &first.task_id,
                "completed",
                "ВИЗУАЛЬНО СООТВЕТСТВУЕТ макету",
                json!({}),
                Some("bad-report-russian-uppercase"),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("text-only principal"));
    }

    #[tokio::test]
    async fn concurrent_assignment_replays_one_task_and_one_created_event() {
        let engine = engine();
        let metadata = json!({"source":"concurrency-test"});
        let first = engine.workflow_assign_task(
            "seat-senior",
            "junior",
            "Concurrent narrow patch",
            "Edit one selector",
            None,
            None,
            &[],
            &metadata,
            Some("concurrent-assignment"),
        );
        let second = engine.workflow_assign_task(
            "seat-senior",
            "junior",
            "Concurrent narrow patch",
            "Edit one selector",
            None,
            None,
            &[],
            &metadata,
            Some("concurrent-assignment"),
        );

        let (first, second) = tokio::join!(first, second);
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.task_id, second.task_id);
        assert_eq!(first.metadata["source"], "concurrency-test");
        let events = engine
            .workflow_task_events("seat-senior", &first.task_id, 20)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == TaskEventKind::Created)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn terminal_tasks_cannot_be_reopened_or_rewritten() {
        let engine = engine();
        let task = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Blocked patch",
                "Missing exact input",
                None,
                None,
                &[],
                &json!({}),
                Some("blocked-assignment"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-junior",
                &task.task_id,
                "Checking",
                Some("blocked-start"),
            )
            .await
            .unwrap();
        let (_, first_terminal) = engine
            .workflow_report_task(
                "seat-junior",
                &task.task_id,
                "blocked",
                "Exact replacement value is missing",
                json!({}),
                Some("blocked-report"),
            )
            .await
            .unwrap();

        let (_, replayed_terminal) = engine
            .workflow_report_task(
                "seat-junior",
                &task.task_id,
                "blocked",
                "Exact replacement value is missing",
                json!({}),
                Some("blocked-report"),
            )
            .await
            .unwrap();
        assert_eq!(first_terminal.event_id, replayed_terminal.event_id);

        let duplicate = engine
            .workflow_report_task(
                "seat-junior",
                &task.task_id,
                "blocked",
                "Exact replacement value is missing",
                json!({}),
                Some("second-terminal-report"),
            )
            .await
            .unwrap_err();
        assert!(duplicate.to_string().contains("original idempotency_key"));

        let restart = engine
            .workflow_start_task(
                "seat-junior",
                &task.task_id,
                "Retrying without a new task",
                Some("invalid-restart"),
            )
            .await
            .unwrap_err();
        assert!(restart.to_string().contains("terminal task"));

        let rewrite = engine
            .workflow_report_task(
                "seat-junior",
                &task.task_id,
                "completed",
                "Pretend it later succeeded",
                json!({}),
                Some("invalid-terminal-rewrite"),
            )
            .await
            .unwrap_err();
        assert!(rewrite.to_string().contains("cannot transition"));
    }

    #[tokio::test]
    async fn legacy_crud_cannot_bypass_workflow_history() {
        let engine = engine();
        let task = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Immutable workflow task",
                "Canonical assignment body",
                None,
                None,
                &[],
                &json!({}),
                Some("immutable-assignment"),
            )
            .await
            .unwrap();

        let update = engine
            .task_update(
                "seat-junior",
                &task.task_id,
                None,
                Some("silently replaced body"),
                None,
                None,
                None,
                Some("completed"),
                Some(&json!({"issuer":"attacker"})),
            )
            .await
            .unwrap_err();
        assert!(update.to_string().contains("append-only"));

        let rename = engine
            .rename_document("seat-junior", &task.task_id, "rewritten-task-id", None)
            .await
            .unwrap_err();
        assert!(rename.to_string().contains("immutable"));

        let deletion = engine
            .task_delete("seat-junior", &task.task_id)
            .await
            .unwrap_err();
        assert!(deletion.to_string().contains("cannot be deleted"));

        let preserved = engine
            .workflow_get_task("seat-senior", &task.task_id)
            .await
            .unwrap();
        assert_eq!(preserved.description, "Canonical assignment body");
        assert_eq!(preserved.status, crate::tasks::STATUS_PENDING);
        assert_eq!(preserved.issuer.as_deref(), Some("senior"));
    }
}
