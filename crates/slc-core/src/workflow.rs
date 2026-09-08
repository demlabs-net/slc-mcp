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
    STATUS_ACTIVE, STATUS_BLOCKED, STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED,
    STATUS_PENDING, TaskInfo, doc_to_task, is_workflow_task, normalize_task_status,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

const TASK_EVENTS: &str = "task_events_v1";
const TASK_EVENT_IDEMPOTENCY: &str = "task_event_idempotency_v1";
const TASK_ASSIGN_IDEMPOTENCY: &str = "task_assign_idempotency_v1";
const MAX_ASSIGNEE_QUEUE_DEPTH: usize = 5_000;
const LEADS_PORTFOLIO_PROJECT: &str = "leads_mass_redesign";

pub const QUEUE_STATE_QUEUED: &str = "queued";
pub const QUEUE_STATE_READY: &str = "ready";
pub const QUEUE_STATE_RUNNING: &str = "running";
pub const QUEUE_STATE_TERMINAL: &str = "terminal";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskEventKind {
    Created,
    Ready,
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
            Self::Ready => "ready",
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

#[derive(Debug, Clone, Serialize)]
pub struct TaskQueueEntry {
    /// Zero is the reserved lane (ready or running); queued work starts at one.
    pub position: usize,
    pub task: TaskInfo,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskQueueSnapshot {
    pub assignee: String,
    /// One Hermes profile is one writer even when its inference backend has
    /// several global slots shared with other role profiles.
    pub capacity: usize,
    pub running_task_id: Option<String>,
    pub ready_task_id: Option<String>,
    pub entries: Vec<TaskQueueEntry>,
}

impl TaskQueueSnapshot {
    pub fn ready_task(&self) -> Option<TaskInfo> {
        self.entries
            .iter()
            .find(|entry| entry.task.queue_state.as_deref() == Some(QUEUE_STATE_READY))
            .map(|entry| entry.task.clone())
    }
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

fn validate_manager_portfolio_metadata(
    actor: &str,
    project_id: Option<&str>,
    metadata: &Map<String, Value>,
) -> SlcResult<()> {
    if actor != "manager" || project_id != Some(LEADS_PORTFOLIO_PROJECT) {
        return Ok(());
    }

    for field in ["pipeline", "slug", "stage", "artifact_root"] {
        if metadata
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(SlcError::InvalidInput(format!(
                "manager-issued {LEADS_PORTFOLIO_PROJECT} tasks require non-empty metadata.{field}"
            )));
        }
    }
    if metadata.get("pipeline").and_then(Value::as_str) != Some(LEADS_PORTFOLIO_PROJECT) {
        return Err(SlcError::InvalidInput(format!(
            "metadata.pipeline must equal {LEADS_PORTFOLIO_PROJECT}"
        )));
    }
    if !matches!(
        metadata.get("stage").and_then(Value::as_str),
        Some("DESIGN" | "BUILD" | "TEST" | "DEPLOY")
    ) {
        return Err(SlcError::InvalidInput(
            "metadata.stage must be DESIGN, BUILD, TEST, or DEPLOY".into(),
        ));
    }
    if metadata.get("attempt").and_then(Value::as_u64).unwrap_or(0) < 1 {
        return Err(SlcError::InvalidInput(
            "manager-issued leads_mass_redesign tasks require metadata.attempt >= 1".into(),
        ));
    }
    if !metadata
        .get("artifact_root")
        .and_then(Value::as_str)
        .is_some_and(|value| value.starts_with('/'))
    {
        return Err(SlcError::InvalidInput(
            "metadata.artifact_root must be an absolute path".into(),
        ));
    }
    Ok(())
}

fn idempotency_key(actor: &str, supplied: &str) -> String {
    content_hash(&format!("{actor}\0{supplied}"))
}

fn deterministic_event_id(actor: &str, supplied: &str) -> String {
    format!("task_event_{}", idempotency_key(actor, supplied))
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

fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        STATUS_COMPLETED | STATUS_BLOCKED | STATUS_FAILED | STATUS_CANCELLED
    )
}

fn effective_queue_state(doc: &Document) -> &'static str {
    let status = workflow_string(doc, "status").unwrap_or_else(|| STATUS_PENDING.to_string());
    if is_terminal_status(&status) {
        return QUEUE_STATE_TERMINAL;
    }
    if status == STATUS_ACTIVE {
        return QUEUE_STATE_RUNNING;
    }
    match workflow_string(doc, "queue_state").as_deref() {
        Some(QUEUE_STATE_READY) => QUEUE_STATE_READY,
        Some(QUEUE_STATE_RUNNING) => QUEUE_STATE_RUNNING,
        _ => QUEUE_STATE_QUEUED,
    }
}

fn queue_order(doc: &Document) -> String {
    workflow_string(doc, "queue_order")
        .or_else(|| workflow_string(doc, "assigned_at"))
        .unwrap_or_else(|| doc.created_at.to_rfc3339())
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
        if let Some(existing) = self
            .store()
            .get_record(TASK_EVENT_IDEMPOTENCY, &key)
            .await?
        {
            if existing.get("fingerprint").and_then(Value::as_str) != Some(fingerprint) {
                return Err(SlcError::InvalidInput(
                    "idempotency_key was already used for a different task event".into(),
                ));
            }
            let event_id = existing
                .get("event_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    SlcError::Storage("task event idempotency record is corrupt".into())
                })?;
            let event = self
                .store()
                .get_record(TASK_EVENTS, event_id)
                .await?
                .ok_or_else(|| SlcError::Storage("idempotent task event is missing".into()))?;
            return Ok(Some(serde_json::from_value(event)?));
        }

        // The event and its idempotency index live in a portable backend
        // abstraction that cannot offer a cross-record transaction. A stable
        // event ID lets a retry repair the index after a crash between writes
        // without appending a second immutable event.
        let event_id = deterministic_event_id(actor, &supplied);
        let Some(value) = self.store().get_record(TASK_EVENTS, &event_id).await? else {
            return Ok(None);
        };
        let event: TaskEvent = serde_json::from_value(value)?;
        let stored_fingerprint = task_event_fingerprint(
            &event.task_id,
            event.kind,
            &event.actor,
            event.recipient.as_deref(),
            event.status.as_deref(),
            &event.message,
            &event.metadata,
        )?;
        if stored_fingerprint != fingerprint {
            return Err(SlcError::InvalidInput(
                "idempotency_key was already used for a different task event".into(),
            ));
        }
        self.store()
            .put_record(
                TASK_EVENT_IDEMPOTENCY,
                &key,
                &json!({"fingerprint": fingerprint, "event_id": event.event_id}),
            )
            .await?;
        Ok(Some(event))
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

    async fn active_task_descends_from(
        &self,
        seat_id: &str,
        ancestor_task_id: &str,
    ) -> SlcResult<bool> {
        let principal = self.workflow_principal(seat_id);
        let Some(active) = self.task_get_active(seat_id).await? else {
            return Ok(false);
        };
        if active.assignee.as_deref() != Some(principal.as_str()) {
            return Ok(false);
        }

        let mut current = active.parent_task_id;
        let mut visited = HashSet::new();
        for _ in 0..64 {
            let Some(task_id) = current else {
                return Ok(false);
            };
            if task_id == ancestor_task_id {
                return Ok(true);
            }
            if !visited.insert(task_id.clone()) {
                return Ok(false);
            }
            let Some(doc) = self.store().kb_get(&task_id).await? else {
                return Ok(false);
            };
            if doc.category != DocumentCategory::Task || !is_workflow_task(&doc) {
                return Ok(false);
            }
            current = workflow_string(&doc, "parent_task_id");
        }
        Ok(false)
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

    async fn workflow_read_task_document(
        &self,
        seat_id: &str,
        task_id: &str,
    ) -> SlcResult<Document> {
        let doc = task_document(self.store().kb_get(task_id).await?, task_id)?;
        if !self.task_access_allowed(seat_id, &doc)
            && !self
                .active_task_descends_from(seat_id, task_id)
                .await?
        {
            return Err(SlcError::PermissionDenied(format!(
                "principal {} cannot read task {task_id}",
                self.workflow_principal(seat_id)
            )));
        }
        Ok(doc)
    }

    /// Resolve a missing task argument from the caller's active SLC task.
    ///
    /// Hermes' deferred MCP bridge can occasionally preserve the tool call's
    /// idempotency key while dropping the required `task_id` field.  A role
    /// has at most one active task pointer, so using it is unambiguous and
    /// keeps the workflow lane safe.  We deliberately do not guess from the
    /// global queue here: callers must have explicitly activated the task (or
    /// have started it already), and a missing pointer remains a clear input
    /// error rather than silently selecting unrelated work.
    async fn workflow_task_id_or_active(
        &self,
        seat_id: &str,
        task_id: &str,
    ) -> SlcResult<String> {
        let task_id = task_id.trim();
        if !task_id.is_empty() {
            return Ok(task_id.to_string());
        }

        let actor = self.workflow_principal(seat_id);
        let active = self
            .task_get_active(seat_id)
            .await?
            .ok_or_else(|| {
                SlcError::InvalidInput(
                    "task_id must not be empty (no active SLC task to resolve)".into(),
                )
            })?;
        if active.assignee.as_deref() != Some(actor.as_str()) {
            return Err(SlcError::PermissionDenied(
                "the active SLC task is not assigned to the caller".into(),
            ));
        }
        Ok(active.task_id)
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

        let supplied_idempotency_key = supplied_idempotency_key
            .map(|supplied| clean_required(supplied, "idempotency_key", 200))
            .transpose()?;
        let now = Utc::now();
        let event = TaskEvent {
            event_id: supplied_idempotency_key.as_deref().map_or_else(
                || format!("task_event_{}", Uuid::new_v4().simple()),
                |supplied| deterministic_event_id(actor, supplied),
            ),
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
        if let Some(supplied) = supplied_idempotency_key.as_deref() {
            let key = idempotency_key(actor, supplied);
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
        let same_event =
            workflow_string(&doc, "last_event_id").as_deref() == Some(event.event_id.as_str());
        let current_event_at = workflow_string(&doc, "last_event_at")
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok());
        let incoming_event_at = DateTime::parse_from_rfc3339(&event.created_at).ok();
        let stale_event = same_event
            || current_event_at
            .zip(incoming_event_at)
            .is_some_and(|(current, incoming)| current >= incoming);
        let mut changed = false;
        if !stale_event {
            doc.metadata
                .extra
                .insert("last_event_id".into(), json!(event.event_id));
            doc.metadata
                .extra
                .insert("last_event_at".into(), json!(event.created_at));
            changed = true;
        }

        // Event storage and the portable document projection cannot share a
        // backend transaction. A retry therefore merges the intended state
        // transition even when a newer message already owns `last_event_*`.
        // Statuses only move forward: PENDING -> IN_WORK -> terminal.
        let current_status = workflow_string(&doc, "status")
            .unwrap_or_else(|| STATUS_PENDING.to_string());
        let mut resulting_status = current_status.clone();
        if let Some(status) = status {
            let apply_status = current_status != status
                && !is_terminal_status(&current_status)
                && (is_terminal_status(status)
                    || (status == STATUS_ACTIVE && current_status == STATUS_PENDING));
            if apply_status {
                doc.metadata.extra.insert("status".into(), json!(status));
                resulting_status = status.to_string();
                changed = true;
            }
            if is_terminal_status(status)
                && resulting_status == status
                && workflow_string(&doc, "terminal_at").is_none()
            {
                doc.metadata
                    .extra
                    .insert("terminal_at".into(), json!(event.created_at));
                changed = true;
            }
        }
        if let Some(summary) = terminal_summary
            && status.is_some_and(|status| resulting_status == status)
            && workflow_string(&doc, "terminal_summary").as_deref() != Some(summary)
        {
            doc.metadata
                .extra
                .insert("terminal_summary".into(), json!(summary));
            changed = true;
        }
        if !changed {
            return Ok(doc_to_task(&doc));
        }
        doc.updated_at = Utc::now();
        doc.version += 1;
        self.store().kb_replace(&doc).await?;
        Ok(doc_to_task(&doc))
    }

    async fn set_task_queue_projection(
        &self,
        task_id: &str,
        state: &str,
        ready_event_id: Option<&str>,
    ) -> SlcResult<TaskInfo> {
        let mut doc = task_document(self.store().kb_get(task_id).await?, task_id)?;
        let current_state = workflow_string(&doc, "queue_state");
        let current_ready_event = workflow_string(&doc, "queue_ready_event_id");
        if current_state.as_deref() == Some(state)
            && current_ready_event.as_deref() == ready_event_id
        {
            return Ok(doc_to_task(&doc));
        }
        doc.metadata
            .extra
            .insert("queue_state".into(), json!(state));
        if let Some(event_id) = ready_event_id {
            doc.metadata
                .extra
                .insert("queue_ready_event_id".into(), json!(event_id));
        } else {
            doc.metadata.extra.remove("queue_ready_event_id");
        }
        doc.updated_at = Utc::now();
        doc.version += 1;
        self.store().kb_replace(&doc).await?;
        Ok(doc_to_task(&doc))
    }

    async fn assignee_queue_documents(&self, assignee: &str) -> SlcResult<Vec<Document>> {
        let assignee_seat = self.workflow_seat(assignee).ok_or_else(|| {
            SlcError::InvalidInput(format!("unknown workflow principal: {assignee}"))
        })?;
        let mut docs = self
            .store()
            .kb_find(
                &DocFilter {
                    category: Some(DocumentCategory::Task),
                    seat_id: Some(assignee_seat),
                    extra_strings: BTreeMap::from([(
                        "assignee".to_string(),
                        assignee.to_string(),
                    )]),
                    extra_strings_not_in: BTreeMap::from([(
                        "status".to_string(),
                        vec![
                            STATUS_COMPLETED.to_string(),
                            STATUS_BLOCKED.to_string(),
                            STATUS_FAILED.to_string(),
                            STATUS_CANCELLED.to_string(),
                        ],
                    )]),
                    ..Default::default()
                },
                &DocSort::by_created(SortDir::Asc),
                MAX_ASSIGNEE_QUEUE_DEPTH + 1,
            )
            .await?
            .into_iter()
            .filter(is_workflow_task)
            .filter(|doc| workflow_string(doc, "assignee").as_deref() == Some(assignee))
            .filter(|doc| effective_queue_state(doc) != QUEUE_STATE_TERMINAL)
            .collect::<Vec<_>>();
        if docs.len() > MAX_ASSIGNEE_QUEUE_DEPTH {
            return Err(SlcError::Limit(format!(
                "task queue for {assignee} exceeds the supported depth of {MAX_ASSIGNEE_QUEUE_DEPTH}; close or archive work before assigning more"
            )));
        }
        docs.sort_by(|left, right| {
            queue_order(left)
                .cmp(&queue_order(right))
                .then_with(|| left.document_id.cmp(&right.document_id))
        });
        Ok(docs)
    }

    /// Reconcile one principal's FIFO while the caller holds `workflow_lock`.
    /// Exactly one non-terminal task may reserve the lane as ready/running.
    async fn reconcile_task_queue_locked(
        &self,
        assignee: &str,
    ) -> SlcResult<TaskQueueSnapshot> {
        let mut docs = self.assignee_queue_documents(assignee).await?;
        let running = docs
            .iter()
            .filter(|doc| effective_queue_state(doc) == QUEUE_STATE_RUNNING)
            .map(|doc| doc.document_id.clone())
            .collect::<Vec<_>>();
        if running.len() > 1 {
            return Err(SlcError::Storage(format!(
                "task queue invariant violated for {assignee}: multiple running writers ({})",
                running.join(", ")
            )));
        }
        let ready = docs
            .iter()
            .filter(|doc| effective_queue_state(doc) == QUEUE_STATE_READY)
            .map(|doc| doc.document_id.clone())
            .collect::<Vec<_>>();
        let oldest_task_id = docs.first().map(|doc| doc.document_id.as_str());
        let preserved = running.first().cloned().or_else(|| {
            oldest_task_id
                .filter(|task_id| ready.iter().any(|ready_id| ready_id == *task_id))
                .map(String::from)
        });
        let duplicate_reservations = ready
            .into_iter()
            .filter(|task_id| Some(task_id.as_str()) != preserved.as_deref())
            .collect::<Vec<_>>();
        for task_id in &duplicate_reservations {
            let repair_key = format!(
                "queue-repair-v1:{task_id}:{}",
                preserved.as_deref().unwrap_or("none")
            );
            let event = self
                .append_task_event(
                    task_id,
                    TaskEventKind::StatusChanged,
                    "slc-queue",
                    Some(assignee),
                    Some(STATUS_PENDING),
                    "Duplicate lane reservation demoted during FIFO reconciliation",
                    json!({"preserved_task_id": preserved.as_deref()}),
                    Some(&repair_key),
                )
                .await?;
            self.touch_task_projection(task_id, &event, None, None)
                .await?;
            self.set_task_queue_projection(task_id, QUEUE_STATE_QUEUED, None)
                .await?;
        }
        if !duplicate_reservations.is_empty() {
            docs = self.assignee_queue_documents(assignee).await?;
        }

        if preserved.is_none()
            && let Some(head) = docs.first()
        {
            let task_id = head.document_id.clone();
            let promotion_key = format!("queue-ready-v1:{task_id}");
            let event = self
                .append_task_event(
                    &task_id,
                    TaskEventKind::Ready,
                    "slc-queue",
                    Some(assignee),
                    Some(STATUS_PENDING),
                    "Task reached the head of the assignee FIFO",
                    json!({"queue_order": queue_order(head)}),
                    Some(&promotion_key),
                )
                .await?;
            self.touch_task_projection(&task_id, &event, None, None)
                .await?;
            self.set_task_queue_projection(
                &task_id,
                QUEUE_STATE_READY,
                Some(&event.event_id),
            )
            .await?;
            docs = self.assignee_queue_documents(assignee).await?;
        }

        let reserved_id = docs
            .iter()
            .find(|doc| {
                matches!(
                    effective_queue_state(doc),
                    QUEUE_STATE_READY | QUEUE_STATE_RUNNING
                )
            })
            .map(|doc| doc.document_id.clone());
        let mut running_task_id = None;
        let mut ready_task_id = None;
        let mut entries = Vec::with_capacity(docs.len());
        let mut queued_position = 1usize;

        if let Some(reserved_id) = reserved_id.as_deref()
            && let Some(doc) = docs.iter().find(|doc| doc.document_id == reserved_id)
        {
            let state = effective_queue_state(doc);
            let task = self
                .set_task_queue_projection(
                    &doc.document_id,
                    state,
                    workflow_string(doc, "queue_ready_event_id").as_deref(),
                )
                .await?;
            if state == QUEUE_STATE_RUNNING {
                running_task_id = Some(doc.document_id.clone());
            } else {
                ready_task_id = Some(doc.document_id.clone());
            }
            entries.push(TaskQueueEntry { position: 0, task });
        }

        for doc in docs
            .iter()
            .filter(|doc| Some(doc.document_id.as_str()) != reserved_id.as_deref())
        {
            let task = self
                .set_task_queue_projection(
                    &doc.document_id,
                    QUEUE_STATE_QUEUED,
                    None,
                )
                .await?;
            entries.push(TaskQueueEntry {
                position: queued_position,
                task,
            });
            queued_position += 1;
        }

        Ok(TaskQueueSnapshot {
            assignee: assignee.to_string(),
            capacity: 1,
            running_task_id,
            ready_task_id,
            entries,
        })
    }

    /// Atomically repair/read an assignee queue and promote its oldest task
    /// only when no ready/running task already reserves the role profile.
    pub async fn workflow_reconcile_task_queue(
        &self,
        seat_id: &str,
        assignee: &str,
    ) -> SlcResult<TaskQueueSnapshot> {
        let _workflow_guard = self.workflow_lock.lock().await;
        let assignee = clean_required(assignee, "assignee", 128)?;
        let actor = self.workflow_principal(seat_id);
        if actor != assignee && !self.can_assign_task(seat_id, &assignee) {
            return Err(SlcError::PermissionDenied(format!(
                "principal {actor} cannot reconcile the {assignee} task queue"
            )));
        }
        if self.workflow_seat(&assignee).is_none() {
            return Err(SlcError::InvalidInput(format!(
                "unknown workflow principal: {assignee}"
            )));
        }
        self.reconcile_task_queue_locked(&assignee).await
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
        let clean_name = clean_required(name, "name", 500)?;
        let caller_metadata = metadata
            .as_object()
            .cloned()
            .ok_or_else(|| SlcError::InvalidInput("metadata must be a JSON object".into()))?;
        validate_manager_portfolio_metadata(&actor, project_id, &caller_metadata)?;

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
        let assignment_idempotency = supplied_idempotency_key
            .map(|supplied| clean_required(supplied, "idempotency_key", 200))
            .transpose()?
            .map(|supplied| {
                let key = idempotency_key(&actor, &supplied);
                (supplied, key)
            });
        if let Some((_, key)) = assignment_idempotency.as_ref() {
            if let Some(existing) = self
                .store()
                .get_record(TASK_ASSIGN_IDEMPOTENCY, key)
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
                // Idempotency survives terminal closure. Terminal tasks are no
                // longer present in the runnable FIFO, but replaying the
                // original assignment must still return the original task
                // projection instead of manufacturing a replacement.
                let task = self.workflow_task_document(seat_id, task_id).await?;
                if effective_queue_state(&task) == QUEUE_STATE_TERMINAL {
                    return Ok(doc_to_task(&task));
                }
                let queue = self.reconcile_task_queue_locked(&assignee).await?;
                return queue
                    .entries
                    .into_iter()
                    .find(|entry| entry.task.task_id == task_id)
                    .map(|entry| entry.task)
                    .ok_or_else(|| {
                        SlcError::Storage(
                            "idempotent assignment disappeared from its task queue".into(),
                        )
                    });
            }

            // Repair the portable-backend crash window in which the task
            // document landed but its assignment idempotency index did not.
            let mut recovered = self
                .store()
                .kb_find(
                    &DocFilter {
                        category: Some(DocumentCategory::Task),
                        seat_id: Some(target_seat.clone()),
                        extra_strings: BTreeMap::from([(
                            "assignment_idempotency_key".to_string(),
                            key.clone(),
                        )]),
                        ..Default::default()
                    },
                    &DocSort::by_created(SortDir::Asc),
                    2,
                )
                .await?
                .into_iter()
                .filter(is_workflow_task)
                .collect::<Vec<_>>();
            if recovered.len() > 1 {
                return Err(SlcError::Storage(format!(
                    "assignment idempotency invariant violated for key {key}"
                )));
            }
            if let Some(recovered_doc) = recovered.pop() {
                if workflow_string(&recovered_doc, "assignment_fingerprint").as_deref()
                    != Some(fingerprint.as_str())
                {
                    return Err(SlcError::InvalidInput(
                        "idempotency_key was already used for a different task assignment".into(),
                    ));
                }
                let created_event_key = format!("assignment-created-v1:{key}");
                let event = self
                    .append_task_event(
                        &recovered_doc.document_id,
                        TaskEventKind::Created,
                        &actor,
                        Some(&assignee),
                        Some(STATUS_PENDING),
                        "Task assigned",
                        json!({"parent_task_id": parent_task_id}),
                        Some(&created_event_key),
                    )
                    .await?;
                self.touch_task_projection(&recovered_doc.document_id, &event, None, None)
                    .await?;
                self.store()
                    .put_record(
                        TASK_ASSIGN_IDEMPOTENCY,
                        key,
                        &json!({
                            "fingerprint": fingerprint,
                            "task_id": recovered_doc.document_id,
                        }),
                    )
                    .await?;
                if effective_queue_state(&recovered_doc) == QUEUE_STATE_TERMINAL {
                    let recovered = self
                        .workflow_task_document(seat_id, &recovered_doc.document_id)
                        .await?;
                    return Ok(doc_to_task(&recovered));
                }
                let queue = self.reconcile_task_queue_locked(&assignee).await?;
                return queue
                    .entries
                    .into_iter()
                    .find(|entry| entry.task.task_id == recovered_doc.document_id)
                    .map(|entry| entry.task)
                    .ok_or_else(|| {
                        SlcError::Storage(
                            "recovered assignment disappeared from its task queue".into(),
                        )
                    });
            }
        }

        // Idempotent replay above is independent of the assignee's current
        // lane health. Once an assignment is durable, unrelated later queue
        // corruption must not make the original request unreplayable. Only a
        // genuinely new assignment validates lineage and repairs/reserves the
        // live FIFO before creating its document.
        let parent = if let Some(parent_id) = parent_task_id {
            Some(self.workflow_task_document(seat_id, parent_id).await?)
        } else {
            None
        };
        let root_task_id = parent.as_ref().map(|doc| {
            workflow_string(doc, "root_task_id").unwrap_or_else(|| doc.document_id.clone())
        });
        let existing_queue = self.reconcile_task_queue_locked(&assignee).await?;
        if existing_queue.entries.len() >= MAX_ASSIGNEE_QUEUE_DEPTH {
            return Err(SlcError::Limit(format!(
                "task queue for {assignee} reached its supported depth of {MAX_ASSIGNEE_QUEUE_DEPTH}; close work before assigning more"
            )));
        }

        let mut workflow_metadata = Map::new();
        workflow_metadata.insert("workflow_metadata".into(), Value::Object(caller_metadata));
        workflow_metadata.insert("workflow_version".into(), json!(1));
        workflow_metadata.insert("issuer".into(), json!(actor));
        workflow_metadata.insert("assignee".into(), json!(assignee));
        let assigned_at = Utc::now();
        workflow_metadata.insert("assigned_at".into(), json!(assigned_at.to_rfc3339()));
        workflow_metadata.insert(
            "queue_order".into(),
            json!(format!(
                "{:020}-{}",
                assigned_at.timestamp_nanos_opt().unwrap_or_default(),
                Uuid::new_v4().simple()
            )),
        );
        workflow_metadata.insert("queue_state".into(), json!(QUEUE_STATE_QUEUED));
        if let Some((_, key)) = assignment_idempotency.as_ref() {
            workflow_metadata.insert("assignment_idempotency_key".into(), json!(key));
            workflow_metadata.insert("assignment_fingerprint".into(), json!(fingerprint));
        }
        if let Some(parent_id) = parent_task_id {
            workflow_metadata.insert("parent_task_id".into(), json!(parent_id));
        }
        if let Some(root_id) = root_task_id.as_deref() {
            workflow_metadata.insert("root_task_id".into(), json!(root_id));
        }

        let mut effective_auto_load = auto_load.to_vec();
        if let Some(policy_document) = self.config.principal_policy_documents.get(&assignee)
            && !effective_auto_load.iter().any(|item| item == policy_document)
        {
            effective_auto_load.push(policy_document.clone());
        }

        let task = self
            .task_create(
                &target_seat,
                &clean_name,
                description,
                project_id,
                &effective_auto_load,
                &Value::Object(workflow_metadata),
            )
            .await?;
        let created_event_key = assignment_idempotency
            .as_ref()
            .map(|(_, key)| format!("assignment-created-v1:{key}"));
        let event = self
            .append_task_event(
                &task.task_id,
                TaskEventKind::Created,
                &actor,
                Some(&assignee),
                Some(crate::tasks::STATUS_PENDING),
                "Task assigned",
                json!({"parent_task_id": parent_task_id}),
                created_event_key.as_deref(),
            )
            .await?;
        let task = self
            .touch_task_projection(&task.task_id, &event, None, None)
            .await?;
        if let Some((_, key)) = assignment_idempotency.as_ref() {
            self.store()
                .put_record(
                    TASK_ASSIGN_IDEMPOTENCY,
                    key,
                    &json!({"fingerprint": fingerprint, "task_id": task.task_id}),
                )
                .await?;
        }
        let queue = self.reconcile_task_queue_locked(&assignee).await?;
        queue
            .entries
            .into_iter()
            .find(|entry| entry.task.task_id == task.task_id)
            .map(|entry| entry.task)
            .ok_or_else(|| SlcError::Storage("assigned task disappeared from its queue".into()))
    }

    pub async fn workflow_get_task(&self, seat_id: &str, task_id: &str) -> SlcResult<TaskInfo> {
        let doc = self.workflow_read_task_document(seat_id, task_id).await?;
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
        if matches!(scope, TaskListScope::Assigned)
            && assignee.is_some_and(|value| value != principal)
        {
            return Ok(Vec::new());
        }
        if matches!(scope, TaskListScope::Issued)
            && issuer.is_some_and(|value| value != principal)
        {
            return Ok(Vec::new());
        }
        let effective_assignee = match scope {
            TaskListScope::Assigned => Some(principal.as_str()),
            _ => assignee,
        };
        let effective_issuer = match scope {
            TaskListScope::Issued => Some(principal.as_str()),
            _ => issuer,
        };
        let mut extra_strings = BTreeMap::new();
        if let Some(value) = effective_assignee {
            extra_strings.insert("assignee".to_string(), value.to_string());
        }
        if let Some(value) = effective_issuer {
            extra_strings.insert("issuer".to_string(), value.to_string());
        }
        if let Some(value) = normalized_status {
            extra_strings.insert("status".to_string(), value.to_string());
        }
        let docs = self
            .store()
            .kb_find(
                &DocFilter {
                    category: Some(DocumentCategory::Task),
                    seat_id: effective_assignee.and_then(|value| self.workflow_seat(value)),
                    extra_strings,
                    ..Default::default()
                },
                &DocSort::by_updated(SortDir::Desc),
                5_000,
            )
            .await?;
        let mut tasks = docs
            .into_iter()
            // The workflow API must not expose legacy task documents. They do
            // not have an immutable event stream, so passing their IDs to
            // workflow_get/list_task_events produces misleading
            // document-not-found failures in coordinators.
            .filter(is_workflow_task)
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
        let task_id = self.workflow_task_id_or_active(seat_id, task_id).await?;
        let task_id = task_id.as_str();
        let mut doc = self.workflow_task_document(seat_id, task_id).await?;
        let actor = self.workflow_principal(seat_id);
        if workflow_string(&doc, "assignee").as_deref() != Some(&actor) {
            return Err(SlcError::PermissionDenied(
                "only the assignee can start a task".into(),
            ));
        }
        let current_status = workflow_string(&doc, "status")
            .unwrap_or_else(|| STATUS_PENDING.to_string());
        if is_terminal_status(&current_status) {
            return Err(SlcError::InvalidInput(format!(
                "terminal task cannot be started again: {current_status}"
            )));
        }
        let issuer = workflow_string(&doc, "issuer");
        let start_message = message.trim();
        let fingerprint = task_event_fingerprint(
            task_id,
            TaskEventKind::Started,
            &actor,
            issuer.as_deref(),
            Some(STATUS_ACTIVE),
            start_message,
            &json!({}),
        )?;
        if let Some(event) = self
            .existing_task_event(&actor, idempotency_key, &fingerprint)
            .await?
        {
            let queue = self.reconcile_task_queue_locked(&actor).await?;
            doc = self.workflow_task_document(seat_id, task_id).await?;
            match effective_queue_state(&doc) {
                QUEUE_STATE_READY | QUEUE_STATE_RUNNING => {
                    if queue
                        .running_task_id
                        .as_deref()
                        .or(queue.ready_task_id.as_deref())
                        != Some(task_id)
                    {
                        return Err(SlcError::Storage(format!(
                            "idempotent start recovery for {task_id} conflicts with the reserved FIFO head"
                        )));
                    }
                }
                QUEUE_STATE_QUEUED => {
                    return Err(SlcError::Storage(format!(
                        "idempotent start event for {task_id} cannot bypass the current FIFO head"
                    )));
                }
                _ => {
                    return Err(SlcError::InvalidInput(
                        "terminal task cannot be started again".into(),
                    ));
                }
            }
            let projected = self
                .touch_task_projection(task_id, &event, Some(STATUS_ACTIVE), None)
                .await?;
            if projected.status != STATUS_ACTIVE {
                return Err(SlcError::Storage(format!(
                    "task projection did not apply the start event for {task_id}"
                )));
            }
            let task = self
                .set_task_queue_projection(
                    task_id,
                    QUEUE_STATE_RUNNING,
                    workflow_string(&doc, "queue_ready_event_id").as_deref(),
                )
                .await?;
            let assignee_seat = self.workflow_seat(&actor).ok_or_else(|| {
                SlcError::InvalidInput(format!("unknown workflow principal: {actor}"))
            })?;
            self.task_activate(&assignee_seat, task_id).await?;
            return Ok((task, event));
        }

        let queue = self.reconcile_task_queue_locked(&actor).await?;
        doc = self.workflow_task_document(seat_id, task_id).await?;
        match effective_queue_state(&doc) {
            QUEUE_STATE_READY => {}
            QUEUE_STATE_RUNNING => {
                return Err(SlcError::InvalidInput(
                    "task is already running; retry only with the original idempotency_key".into(),
                ));
            }
            QUEUE_STATE_QUEUED => {
                let position = queue
                    .entries
                    .iter()
                    .find(|entry| entry.task.task_id == task_id)
                    .map(|entry| entry.position);
                return Err(SlcError::InvalidInput(format!(
                    "task is queued at position {}; only queue head {} may start",
                    position.unwrap_or(1),
                    queue
                        .running_task_id
                        .as_deref()
                        .or(queue.ready_task_id.as_deref())
                        .unwrap_or("<none>")
                )));
            }
            _ => {
                return Err(SlcError::InvalidInput(
                    "terminal task cannot be started again".into(),
                ));
            }
        }
        let event = self
            .append_task_event(
                task_id,
                TaskEventKind::Started,
                &actor,
                issuer.as_deref(),
                Some(STATUS_ACTIVE),
                start_message,
                json!({}),
                idempotency_key,
            )
            .await?;
        let projected = self
            .touch_task_projection(task_id, &event, Some(STATUS_ACTIVE), None)
            .await?;
        if projected.status != STATUS_ACTIVE {
            return Err(SlcError::Storage(format!(
                "task projection did not apply the start event for {task_id}"
            )));
        }
        let task = self
            .set_task_queue_projection(
                task_id,
                QUEUE_STATE_RUNNING,
                workflow_string(&doc, "queue_ready_event_id").as_deref(),
            )
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
    ) -> SlcResult<(TaskInfo, TaskEvent, Option<TaskInfo>)> {
        let _workflow_guard = self.workflow_lock.lock().await;
        let task_id = self.workflow_task_id_or_active(seat_id, task_id).await?;
        let task_id = task_id.as_str();
        let doc = self.workflow_task_document(seat_id, task_id).await?;
        let actor = self.workflow_principal(seat_id);
        let assignee = workflow_string(&doc, "assignee")
            .unwrap_or_else(|| doc.seat_id.clone().unwrap_or_default());
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
            .unwrap_or_else(|| STATUS_PENDING.to_string());
        if is_terminal_status(&current_status) {
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
                let repaired = self
                    .set_task_queue_projection(task_id, QUEUE_STATE_TERMINAL, None)
                    .await?;
                if let Some(assignee_seat) = self.workflow_seat(&assignee)
                    && let Some(seat) = self.seats.get_seat(&assignee_seat).await?
                    && seat.active_task_id.as_deref() == Some(task_id)
                {
                    self.seats
                        .set_active_task(&assignee_seat, None, None)
                        .await?;
                }
                let queue = self.reconcile_task_queue_locked(&assignee).await?;
                return Ok((repaired, event, queue.ready_task()));
            }
            return Err(SlcError::InvalidInput(
                "terminal task already has a report; retry only with the original idempotency_key"
                    .into(),
            ));
        }
        if actor != assignee {
            return Err(SlcError::PermissionDenied(
                "only the assignee can report this task; a coordinator must not close a live executor lane"
                    .into(),
            ));
        }
        if effective_queue_state(&doc) != QUEUE_STATE_RUNNING {
            return Err(SlcError::InvalidInput(
                "task must be started from the FIFO head before reporting".into(),
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
        let mut task = self
            .touch_task_projection(
                task_id,
                &event,
                Some(status),
                terminal.then_some(summary.as_str()),
            )
            .await?;
        if task.status != status {
            return Err(SlcError::Storage(format!(
                "task projection did not apply the {status} report for {task_id}"
            )));
        }
        if terminal {
            task = self
                .set_task_queue_projection(task_id, QUEUE_STATE_TERMINAL, None)
                .await?;
        } else if task.queue_state.as_deref() != Some(QUEUE_STATE_RUNNING) {
            task = self
                .set_task_queue_projection(
                    task_id,
                    QUEUE_STATE_RUNNING,
                    task.queue_ready_event_id.as_deref(),
                )
                .await?;
        }
        if terminal
            && let Some(assignee_seat) = self.workflow_seat(&assignee)
            && let Some(seat) = self.seats.get_seat(&assignee_seat).await?
            && seat.active_task_id.as_deref() == Some(task_id)
        {
            self.seats
                .set_active_task(&assignee_seat, None, None)
                .await?;
        }
        let next_ready = if terminal {
            self.reconcile_task_queue_locked(&assignee)
                .await?
                .ready_task()
        } else {
            None
        };
        Ok((task, event, next_ready))
    }

    pub async fn workflow_cancel_task(
        &self,
        seat_id: &str,
        task_id: &str,
        reason: &str,
        metadata: Value,
        idempotency_key: Option<&str>,
    ) -> SlcResult<(TaskInfo, TaskEvent, Option<TaskInfo>)> {
        let _workflow_guard = self.workflow_lock.lock().await;
        let task_id = clean_required(task_id, "task_id", 500)?;
        let doc = self.workflow_task_document(seat_id, &task_id).await?;
        let actor = self.workflow_principal(seat_id);
        let issuer = workflow_string(&doc, "issuer");
        let assignee = workflow_string(&doc, "assignee")
            .unwrap_or_else(|| doc.seat_id.clone().unwrap_or_default());
        let global_coordinator = self
            .config
            .task_assign_acl
            .get(&actor)
            .is_some_and(|targets| targets.contains("*"));
        if issuer.as_deref() != Some(&actor) && assignee != actor && !global_coordinator {
            return Err(SlcError::PermissionDenied(
                "only the issuer, assignee, or a global task coordinator can cancel this task"
                    .into(),
            ));
        }
        let reason = clean_required(reason, "reason", 100_000)?;
        let recipient = if actor == assignee {
            issuer.as_deref()
        } else {
            Some(assignee.as_str())
        };
        let fingerprint = task_event_fingerprint(
            &task_id,
            TaskEventKind::StatusChanged,
            &actor,
            recipient,
            Some(STATUS_CANCELLED),
            &reason,
            &metadata,
        )?;
        let current_status = workflow_string(&doc, "status")
            .unwrap_or_else(|| STATUS_PENDING.to_string());
        if is_terminal_status(&current_status) {
            if current_status != STATUS_CANCELLED
                || workflow_string(&doc, "terminal_summary").as_deref()
                    != Some(reason.as_str())
            {
                return Err(SlcError::InvalidInput(format!(
                    "terminal task cannot transition from {current_status} to {STATUS_CANCELLED}"
                )));
            }
            if let Some(event) = self
                .existing_task_event(&actor, idempotency_key, &fingerprint)
                .await?
            {
                let repaired = self
                    .set_task_queue_projection(&task_id, QUEUE_STATE_TERMINAL, None)
                    .await?;
                if let Some(assignee_seat) = self.workflow_seat(&assignee)
                    && let Some(seat) = self.seats.get_seat(&assignee_seat).await?
                    && seat.active_task_id.as_deref() == Some(task_id.as_str())
                {
                    self.seats
                        .set_active_task(&assignee_seat, None, None)
                        .await?;
                }
                self.reconcile_task_queue_locked(&assignee).await?;
                return Ok((repaired, event, None));
            }
            return Err(SlcError::InvalidInput(
                "cancelled task already has a status event; retry only with the original idempotency_key"
                    .into(),
            ));
        }

        // SLC owns durable task state, not the transport/process lifecycle. A
        // coordinator cancelling a RUNNING projection cannot prove that the
        // corresponding Hermes run has stopped; releasing the lane here could
        // promote a second writer while the first one is still editing. A
        // live assignee must close its run with report_task. After a crashed
        // transport is confirmed idle, wake that same assignee to recover and
        // report the existing task instead of manufacturing a replacement.
        if effective_queue_state(&doc) == QUEUE_STATE_RUNNING {
            return Err(SlcError::InvalidInput(
                "running task cannot be cancelled because SLC does not control its executor; let the assignee report it, or stop the transport and recover the same task"
                    .into(),
            ));
        }

        let released_lane = effective_queue_state(&doc) == QUEUE_STATE_READY;
        let event = self
            .append_task_event(
                &task_id,
                TaskEventKind::StatusChanged,
                &actor,
                recipient,
                Some(STATUS_CANCELLED),
                &reason,
                metadata,
                idempotency_key,
            )
            .await?;
        let mut task = self
            .touch_task_projection(
                &task_id,
                &event,
                Some(STATUS_CANCELLED),
                Some(&reason),
            )
            .await?;
        if task.status != STATUS_CANCELLED {
            return Err(SlcError::Storage(format!(
                "task projection did not apply cancellation for {task_id}"
            )));
        }
        task = self
            .set_task_queue_projection(&task_id, QUEUE_STATE_TERMINAL, None)
            .await?;
        if let Some(assignee_seat) = self.workflow_seat(&assignee)
            && let Some(seat) = self.seats.get_seat(&assignee_seat).await?
            && seat.active_task_id.as_deref() == Some(task_id.as_str())
        {
            self.seats
                .set_active_task(&assignee_seat, None, None)
                .await?;
        }
        let queue = self.reconcile_task_queue_locked(&assignee).await?;
        let next_ready = released_lane.then(|| queue.ready_task()).flatten();
        Ok((task, event, next_ready))
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
        let task_id = self.workflow_task_id_or_active(seat_id, task_id).await?;
        let task_id = task_id.as_str();
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
        self.workflow_read_task_document(seat_id, task_id).await?;
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
    use std::collections::HashMap;
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
            principal_policy_documents: HashMap::from([
                ("senior".into(), "swarm_pipeline_senior_v2".into()),
                ("junior".into(), "swarm_pipeline_junior_v2".into()),
            ]),
            text_only_principals: HashSet::from(["junior".into()]),
            ..Default::default()
        };
        SlcEngine::with(store, Arc::new(MockLlm::new(vec![])), config)
    }

    #[tokio::test]
    async fn manager_portfolio_assignment_requires_structured_identity() {
        let engine = engine();
        let error = engine
            .workflow_assign_task(
                "seat-manager",
                "senior",
                "Build missing identity",
                "Identity exists only in prose",
                None,
                Some(LEADS_PORTFOLIO_PROJECT),
                &[],
                &json!({}),
                Some("missing-portfolio-identity"),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, SlcError::InvalidInput(_)));
        assert!(error.to_string().contains("metadata.pipeline"));

        let task = engine
            .workflow_assign_task(
                "seat-manager",
                "senior",
                "Build valid identity",
                "Structured identity is authoritative",
                None,
                Some(LEADS_PORTFOLIO_PROJECT),
                &[],
                &json!({
                    "pipeline": LEADS_PORTFOLIO_PROJECT,
                    "slug": "valid-identity",
                    "stage": "BUILD",
                    "artifact_root": "/work/shared/leads-mass-redesign/valid-identity/layout",
                    "attempt": 1,
                }),
                Some("valid-portfolio-identity"),
            )
            .await
            .unwrap();

        assert_eq!(task.metadata["slug"], "valid-identity");
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
        assert_eq!(
            child.auto_load,
            vec!["swarm_pipeline_junior_v2".to_string()]
        );
        assert!(
            engine
                .task_get_active("seat-junior")
                .await
                .unwrap()
                .is_none(),
            "assignment queues work but must not steal the assignee's active context"
        );
        assert!(matches!(
            engine
                .workflow_get_task("seat-junior", &parent.task_id)
                .await,
            Err(SlcError::PermissionDenied(_))
        ));

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
        assert_eq!(
            engine
                .workflow_get_task("seat-junior", &parent.task_id)
                .await
                .unwrap()
                .task_id,
            parent.task_id,
            "an active child assignee must be able to inspect its ancestor context"
        );
        assert!(matches!(
            engine
                .workflow_cancel_task(
                    "seat-junior",
                    &parent.task_id,
                    "must remain read-only",
                    json!({}),
                    Some("ancestor-cancel-denied"),
                )
                .await,
            Err(SlcError::PermissionDenied(_))
        ));
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
        let (reported, _, _) = engine
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
        assert_eq!(events.len(), 5);
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
    async fn missing_task_argument_resolves_active_assignee_task() {
        let engine = engine();
        let task = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Active pointer fallback",
                "Use the active task when the bridge drops task_id",
                None,
                None,
                &[],
                &json!({}),
                Some("active-fallback-assignment"),
            )
            .await
            .unwrap();

        // This mirrors the deferred MCP bridge failure seen in live runs:
        // activate_task succeeds, while the subsequent start_task payload
        // loses its required task_id but retains its idempotency key.
        engine
            .task_activate("seat-junior", &task.task_id)
            .await
            .unwrap();
        let (started, _) = engine
            .workflow_start_task(
                "seat-junior",
                "",
                "Started via active task",
                Some("active-fallback-start"),
            )
            .await
            .unwrap();
        assert_eq!(started.task_id, task.task_id);
        assert_eq!(started.status, STATUS_ACTIVE);

        let (reported, _, _) = engine
            .workflow_report_task(
                "seat-junior",
                "",
                "completed",
                "Machine checks passed; visual review remains external",
                json!({}),
                Some("active-fallback-report"),
            )
            .await
            .unwrap();
        assert_eq!(reported.task_id, task.task_id);
        assert_eq!(reported.status, STATUS_COMPLETED);
        assert!(engine.task_get_active("seat-junior").await.unwrap().is_none());
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
        engine
            .workflow_start_task(
                "seat-junior",
                &first.task_id,
                "Starting narrow patch",
                Some("visual-policy-start"),
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
    async fn assignment_idempotency_survives_terminal_queue_removal() {
        let engine = engine();
        let first = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Terminal replay",
                "One bounded change",
                None,
                None,
                &[],
                &json!({}),
                Some("terminal-assignment"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-junior",
                &first.task_id,
                "Starting",
                Some("terminal-start"),
            )
            .await
            .unwrap();
        engine
            .workflow_report_task(
                "seat-junior",
                &first.task_id,
                "completed",
                "Machine checks passed; visual review remains with the senior",
                json!({}),
                Some("terminal-report"),
            )
            .await
            .unwrap();

        let replay = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Terminal replay",
                "One bounded change",
                None,
                None,
                &[],
                &json!({}),
                Some("terminal-assignment"),
            )
            .await
            .unwrap();
        assert_eq!(replay.task_id, first.task_id);
        assert_eq!(replay.status, STATUS_COMPLETED);
        assert_eq!(
            replay.queue_state.as_deref(),
            Some(QUEUE_STATE_TERMINAL)
        );
    }

    #[tokio::test]
    async fn terminal_assignment_replay_isolated_from_later_queue_corruption() {
        let engine = engine();
        let terminal = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Replay independently",
                "The original durable result must remain replayable",
                None,
                None,
                &[],
                &json!({}),
                Some("isolated-terminal-assignment"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-junior",
                &terminal.task_id,
                "Starting",
                Some("isolated-terminal-start"),
            )
            .await
            .unwrap();
        engine
            .workflow_report_task(
                "seat-junior",
                &terminal.task_id,
                "completed",
                "Machine checks passed",
                json!({}),
                Some("isolated-terminal-report"),
            )
            .await
            .unwrap();

        let later_head = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Later head",
                "Independent later work",
                None,
                None,
                &[],
                &json!({}),
                Some("isolated-later-head"),
            )
            .await
            .unwrap();
        let later_tail = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Later tail",
                "Independent later work",
                None,
                None,
                &[],
                &json!({}),
                Some("isolated-later-tail"),
            )
            .await
            .unwrap();
        engine
            .set_task_queue_projection(&later_head.task_id, QUEUE_STATE_RUNNING, None)
            .await
            .unwrap();
        engine
            .set_task_queue_projection(&later_tail.task_id, QUEUE_STATE_RUNNING, None)
            .await
            .unwrap();

        let replay = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Replay independently",
                "The original durable result must remain replayable",
                None,
                None,
                &[],
                &json!({}),
                Some("isolated-terminal-assignment"),
            )
            .await
            .unwrap();
        assert_eq!(replay.task_id, terminal.task_id);
        assert_eq!(replay.status, STATUS_COMPLETED);

        let queue_error = engine
            .workflow_reconcile_task_queue("seat-senior", "junior")
            .await
            .unwrap_err();
        assert!(queue_error.to_string().contains("multiple running writers"));
    }

    #[tokio::test]
    async fn assignee_fifo_allows_one_writer_and_promotes_in_order() {
        let engine = engine();
        let first = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Patch one",
                "First file",
                None,
                None,
                &[],
                &json!({}),
                Some("fifo-1"),
            )
            .await
            .unwrap();
        let second = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Patch two",
                "Second file",
                None,
                None,
                &[],
                &json!({}),
                Some("fifo-2"),
            )
            .await
            .unwrap();
        let third = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Patch three",
                "Third file",
                None,
                None,
                &[],
                &json!({}),
                Some("fifo-3"),
            )
            .await
            .unwrap();

        assert_eq!(first.queue_state.as_deref(), Some(QUEUE_STATE_READY));
        assert_eq!(second.queue_state.as_deref(), Some(QUEUE_STATE_QUEUED));
        assert_eq!(third.queue_state.as_deref(), Some(QUEUE_STATE_QUEUED));
        let ready_terminal = engine
            .workflow_report_task(
                "seat-junior",
                &first.task_id,
                "completed",
                "Must not close before start",
                json!({}),
                Some("fifo-report-ready-early"),
            )
            .await
            .unwrap_err();
        assert!(ready_terminal.to_string().contains("must be started"));
        let queued_terminal = engine
            .workflow_report_task(
                "seat-junior",
                &second.task_id,
                "blocked",
                "Must not bypass the head",
                json!({}),
                Some("fifo-report-queued-early"),
            )
            .await
            .unwrap_err();
        assert!(queued_terminal.to_string().contains("must be started"));
        let queued_start = engine
            .workflow_start_task(
                "seat-junior",
                &second.task_id,
                "Must not jump FIFO",
                Some("fifo-start-2-early"),
            )
            .await
            .unwrap_err();
        assert!(queued_start.to_string().contains("queued at position 1"));

        engine
            .workflow_start_task(
                "seat-junior",
                &first.task_id,
                "Starting head",
                Some("fifo-start-1"),
            )
            .await
            .unwrap();
        let duplicate_start = engine
            .workflow_start_task(
                "seat-junior",
                &first.task_id,
                "Different start",
                Some("fifo-start-1-duplicate"),
            )
            .await
            .unwrap_err();
        assert!(duplicate_start.to_string().contains("already running"));

        let (closed, _, next) = engine
            .workflow_report_task(
                "seat-junior",
                &first.task_id,
                "completed",
                "Exact patch and checks completed; visual review remains external",
                json!({}),
                Some("fifo-report-1"),
            )
            .await
            .unwrap();
        assert_eq!(closed.queue_state.as_deref(), Some(QUEUE_STATE_TERMINAL));
        assert!(
            closed.queue_ready_event_id.is_none(),
            "terminal projections must not expose a stale runnable wake"
        );
        let next = next.expect("second task must be promoted");
        assert_eq!(next.task_id, second.task_id);
        assert_eq!(next.queue_state.as_deref(), Some(QUEUE_STATE_READY));

        let queue = engine
            .workflow_reconcile_task_queue("seat-senior", "junior")
            .await
            .unwrap();
        assert_eq!(queue.capacity, 1);
        assert_eq!(queue.ready_task_id.as_deref(), Some(second.task_id.as_str()));
        assert_eq!(queue.entries.len(), 2);
        assert_eq!(queue.entries[0].position, 0);
        assert_eq!(queue.entries[0].task.task_id, second.task_id);
        assert_eq!(queue.entries[1].position, 1);
        assert_eq!(queue.entries[1].task.task_id, third.task_id);
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
    async fn event_and_assignment_idempotency_indexes_repair_after_partial_persistence() {
        let engine = engine();
        let task = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Recover portable write",
                "One exact patch",
                None,
                None,
                &[],
                &json!({}),
                Some("recover-assignment"),
            )
            .await
            .unwrap();
        let assignment_key = idempotency_key("senior", "recover-assignment");
        engine
            .set_task_queue_projection(&task.task_id, QUEUE_STATE_QUEUED, None)
            .await
            .unwrap();
        let indexed_replay = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Recover portable write",
                "One exact patch",
                None,
                None,
                &[],
                &json!({}),
                Some("recover-assignment"),
            )
            .await
            .unwrap();
        assert_eq!(indexed_replay.queue_state.as_deref(), Some(QUEUE_STATE_READY));
        engine
            .store()
            .delete_record(TASK_ASSIGN_IDEMPOTENCY, &assignment_key)
            .await
            .unwrap();

        let replay = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Recover portable write",
                "One exact patch",
                None,
                None,
                &[],
                &json!({}),
                Some("recover-assignment"),
            )
            .await
            .unwrap();
        assert_eq!(replay.task_id, task.task_id);

        engine
            .workflow_start_task(
                "seat-junior",
                &task.task_id,
                "Starting",
                Some("recover-start"),
            )
            .await
            .unwrap();
        let first_message = engine
            .workflow_task_message(
                "seat-junior",
                &task.task_id,
                "senior",
                "First durable message",
                json!({}),
                Some("recover-message"),
            )
            .await
            .unwrap();
        let event_key = idempotency_key("junior", "recover-message");
        engine
            .store()
            .delete_record(TASK_EVENT_IDEMPOTENCY, &event_key)
            .await
            .unwrap();
        let replayed_message = engine
            .workflow_task_message(
                "seat-junior",
                &task.task_id,
                "senior",
                "First durable message",
                json!({}),
                Some("recover-message"),
            )
            .await
            .unwrap();
        assert_eq!(replayed_message.event_id, first_message.event_id);
        let events = engine
            .workflow_task_events("seat-senior", &task.task_id, 50)
            .await
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == TaskEventKind::Message)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn old_idempotent_retries_do_not_rewind_projection() {
        let engine = engine();
        let task = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Projection order",
                "Keep the newest event",
                None,
                None,
                &[],
                &json!({}),
                Some("projection-assignment"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-junior",
                &task.task_id,
                "Starting",
                Some("projection-start"),
            )
            .await
            .unwrap();
        let latest = engine
            .workflow_task_message(
                "seat-junior",
                &task.task_id,
                "senior",
                "Newest event",
                json!({}),
                Some("projection-message"),
            )
            .await
            .unwrap();
        let (replayed_start, _) = engine
            .workflow_start_task(
                "seat-junior",
                &task.task_id,
                "Starting",
                Some("projection-start"),
            )
            .await
            .unwrap();
        assert_eq!(
            replayed_start.last_event_id.as_deref(),
            Some(latest.event_id.as_str())
        );
    }

    #[tokio::test]
    async fn terminal_retry_merges_status_without_rewinding_a_newer_message() {
        let engine = engine();
        let task = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Projection crash recovery",
                "Recover a terminal projection",
                None,
                None,
                &[],
                &json!({}),
                Some("projection-crash-assignment"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-junior",
                &task.task_id,
                "Starting",
                Some("projection-crash-start"),
            )
            .await
            .unwrap();

        let summary = "Machine verification complete";
        engine
            .append_task_event(
                &task.task_id,
                TaskEventKind::Report,
                "junior",
                Some("senior"),
                Some(STATUS_COMPLETED),
                summary,
                json!({}),
                Some("projection-crash-report"),
            )
            .await
            .unwrap();
        let latest = engine
            .workflow_task_message(
                "seat-junior",
                &task.task_id,
                "senior",
                "A later message reached the projection first",
                json!({}),
                Some("projection-crash-message"),
            )
            .await
            .unwrap();

        let (repaired, _, _) = engine
            .workflow_report_task(
                "seat-junior",
                &task.task_id,
                "completed",
                summary,
                json!({}),
                Some("projection-crash-report"),
            )
            .await
            .unwrap();
        assert_eq!(repaired.status, STATUS_COMPLETED);
        assert_eq!(
            repaired.last_event_id.as_deref(),
            Some(latest.event_id.as_str()),
            "status recovery must not rewind the latest-event cursor"
        );
        assert_eq!(repaired.queue_state.as_deref(), Some(QUEUE_STATE_TERMINAL));
    }

    #[tokio::test]
    async fn idempotent_start_recovery_cannot_bypass_another_fifo_head() {
        let engine = engine();
        let head = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "FIFO head",
                "Must retain the lane",
                None,
                None,
                &[],
                &json!({}),
                Some("start-conflict-head"),
            )
            .await
            .unwrap();
        let tail = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "FIFO tail",
                "Must not bypass the head",
                None,
                None,
                &[],
                &json!({}),
                Some("start-conflict-tail"),
            )
            .await
            .unwrap();
        engine
            .append_task_event(
                &head.task_id,
                TaskEventKind::Started,
                "junior",
                Some("senior"),
                Some(STATUS_ACTIVE),
                "Starting head",
                json!({}),
                Some("start-head-recovery"),
            )
            .await
            .unwrap();
        let (recovered_head, _) = engine
            .workflow_start_task(
                "seat-junior",
                &head.task_id,
                "Starting head",
                Some("start-head-recovery"),
            )
            .await
            .unwrap();
        assert_eq!(recovered_head.status, STATUS_ACTIVE);
        assert_eq!(
            recovered_head.queue_state.as_deref(),
            Some(QUEUE_STATE_RUNNING)
        );
        engine
            .append_task_event(
                &tail.task_id,
                TaskEventKind::Started,
                "junior",
                Some("senior"),
                Some(STATUS_ACTIVE),
                "Starting tail",
                json!({}),
                Some("start-conflict-event"),
            )
            .await
            .unwrap();

        let error = engine
            .workflow_start_task(
                "seat-junior",
                &tail.task_id,
                "Starting tail",
                Some("start-conflict-event"),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot bypass"));
        let queue = engine
            .workflow_reconcile_task_queue("seat-senior", "junior")
            .await
            .unwrap();
        assert_eq!(queue.running_task_id.as_deref(), Some(head.task_id.as_str()));
        assert_eq!(
            queue
                .entries
                .iter()
                .find(|entry| entry.task.task_id == tail.task_id)
                .unwrap()
                .task
                .queue_state
                .as_deref(),
            Some(QUEUE_STATE_QUEUED)
        );
    }

    #[tokio::test]
    async fn cancellation_removes_queued_work_and_promotes_only_a_released_head() {
        let engine = engine();
        let first = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "First",
                "Ready work",
                None,
                None,
                &[],
                &json!({}),
                Some("cancel-first"),
            )
            .await
            .unwrap();
        let second = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Second",
                "Obsolete queued work",
                None,
                None,
                &[],
                &json!({}),
                Some("cancel-second"),
            )
            .await
            .unwrap();
        let third = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Third",
                "Next valid work",
                None,
                None,
                &[],
                &json!({}),
                Some("cancel-third"),
            )
            .await
            .unwrap();

        let (cancelled_tail, _, tail_next) = engine
            .workflow_cancel_task(
                "seat-senior",
                &second.task_id,
                "Superseded before execution",
                json!({}),
                Some("cancel-tail-event"),
            )
            .await
            .unwrap();
        assert_eq!(cancelled_tail.status, STATUS_CANCELLED);
        assert!(tail_next.is_none(), "cancelling queued work emits no duplicate wake");

        let (cancelled_head, event, next) = engine
            .workflow_cancel_task(
                "seat-senior",
                &first.task_id,
                "No longer required",
                json!({"operator":"senior"}),
                Some("cancel-head-event"),
            )
            .await
            .unwrap();
        assert_eq!(cancelled_head.status, STATUS_CANCELLED);
        assert_eq!(cancelled_head.queue_state.as_deref(), Some(QUEUE_STATE_TERMINAL));
        assert_eq!(next.unwrap().task_id, third.task_id);

        let (replayed, replayed_event, replay_next) = engine
            .workflow_cancel_task(
                "seat-senior",
                &first.task_id,
                "No longer required",
                json!({"operator":"senior"}),
                Some("cancel-head-event"),
            )
            .await
            .unwrap();
        assert_eq!(replayed.status, STATUS_CANCELLED);
        assert_eq!(replayed_event.event_id, event.event_id);
        assert!(replay_next.is_none(), "a replay must not emit a second ready wake");
    }

    #[tokio::test]
    async fn cancellation_cannot_release_a_running_executor_lane() {
        let engine = engine();
        let running = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Running writer",
                "Must retain its lane until its own terminal report",
                None,
                None,
                &[],
                &json!({}),
                Some("cancel-running-head"),
            )
            .await
            .unwrap();
        let queued = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Queued writer",
                "Must not be promoted while the first process is live",
                None,
                None,
                &[],
                &json!({}),
                Some("cancel-running-tail"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-junior",
                &running.task_id,
                "Writer is live",
                Some("cancel-running-start"),
            )
            .await
            .unwrap();

        let coordinator_report = engine
            .workflow_report_task(
                "seat-manager",
                &running.task_id,
                "completed",
                "Coordinator must not close another process",
                json!({}),
                Some("coordinator-terminal-report"),
            )
            .await
            .unwrap_err();
        assert!(coordinator_report
            .to_string()
            .contains("only the assignee can report"));

        for actor in ["seat-senior", "seat-junior", "seat-manager"] {
            let error = engine
                .workflow_cancel_task(
                    actor,
                    &running.task_id,
                    "Do not release a live process",
                    json!({}),
                    Some(&format!("cancel-running-{actor}")),
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("running task cannot be cancelled"));
        }

        let queue = engine
            .workflow_reconcile_task_queue("seat-senior", "junior")
            .await
            .unwrap();
        assert_eq!(
            queue.running_task_id.as_deref(),
            Some(running.task_id.as_str())
        );
        assert!(queue.ready_task_id.is_none());
        assert_eq!(
            queue
                .entries
                .iter()
                .find(|entry| entry.task.task_id == queued.task_id)
                .unwrap()
                .task
                .queue_state
                .as_deref(),
            Some(QUEUE_STATE_QUEUED)
        );
        assert_eq!(
            engine
                .task_get_active("seat-junior")
                .await
                .unwrap()
                .unwrap()
                .task_id,
            running.task_id
        );
    }

    #[tokio::test]
    async fn reconciliation_repairs_duplicate_ready_reservations() {
        let engine = engine();
        let first = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Head",
                "First",
                None,
                None,
                &[],
                &json!({}),
                Some("repair-ready-1"),
            )
            .await
            .unwrap();
        let second = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Tail",
                "Second",
                None,
                None,
                &[],
                &json!({}),
                Some("repair-ready-2"),
            )
            .await
            .unwrap();
        engine
            .set_task_queue_projection(&second.task_id, QUEUE_STATE_READY, Some("stale-wake"))
            .await
            .unwrap();

        let queue = engine
            .workflow_reconcile_task_queue("seat-senior", "junior")
            .await
            .unwrap();
        assert_eq!(queue.ready_task_id.as_deref(), Some(first.task_id.as_str()));
        let repaired = queue
            .entries
            .iter()
            .find(|entry| entry.task.task_id == second.task_id)
            .unwrap();
        assert_eq!(repaired.task.queue_state.as_deref(), Some(QUEUE_STATE_QUEUED));
        assert!(repaired.task.queue_ready_event_id.is_none());
    }

    #[tokio::test]
    async fn reconciliation_replaces_an_out_of_order_ready_reservation() {
        let engine = engine();
        let first = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Oldest queued task",
                "Must regain the head",
                None,
                None,
                &[],
                &json!({}),
                Some("repair-order-1"),
            )
            .await
            .unwrap();
        let second = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Newer task",
                "Must not keep an invalid reservation",
                None,
                None,
                &[],
                &json!({}),
                Some("repair-order-2"),
            )
            .await
            .unwrap();
        engine
            .set_task_queue_projection(&first.task_id, QUEUE_STATE_QUEUED, None)
            .await
            .unwrap();
        engine
            .set_task_queue_projection(&second.task_id, QUEUE_STATE_READY, Some("wrong-head"))
            .await
            .unwrap();

        let queue = engine
            .workflow_reconcile_task_queue("seat-senior", "junior")
            .await
            .unwrap();
        assert_eq!(queue.ready_task_id.as_deref(), Some(first.task_id.as_str()));
        let tail = queue
            .entries
            .iter()
            .find(|entry| entry.task.task_id == second.task_id)
            .unwrap();
        assert_eq!(tail.task.queue_state.as_deref(), Some(QUEUE_STATE_QUEUED));
        assert!(tail.task.queue_ready_event_id.is_none());
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
        let (_, first_terminal, _) = engine
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

        let (_, replayed_terminal, _) = engine
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
    async fn terminal_report_replay_repairs_queue_and_active_anchor() {
        let engine = engine();
        let task = engine
            .workflow_assign_task(
                "seat-senior",
                "junior",
                "Repair terminal projection",
                "Close once",
                None,
                None,
                &[],
                &json!({}),
                Some("terminal-repair-assignment"),
            )
            .await
            .unwrap();
        engine
            .workflow_start_task(
                "seat-junior",
                &task.task_id,
                "Starting",
                Some("terminal-repair-start"),
            )
            .await
            .unwrap();
        engine
            .workflow_report_task(
                "seat-junior",
                &task.task_id,
                "completed",
                "Machine verification complete",
                json!({}),
                Some("terminal-repair-report"),
            )
            .await
            .unwrap();

        let mut stale = engine.store().kb_get(&task.task_id).await.unwrap().unwrap();
        stale
            .metadata
            .extra
            .insert("queue_state".into(), json!(QUEUE_STATE_RUNNING));
        stale
            .metadata
            .extra
            .insert("queue_ready_event_id".into(), json!("stale-ready-event"));
        engine.store().kb_replace(&stale).await.unwrap();
        engine
            .seats
            .set_active_task("seat-junior", Some(&task.task_id), None)
            .await
            .unwrap();

        let (repaired, _, _) = engine
            .workflow_report_task(
                "seat-junior",
                &task.task_id,
                "completed",
                "Machine verification complete",
                json!({}),
                Some("terminal-repair-report"),
            )
            .await
            .unwrap();
        assert_eq!(repaired.queue_state.as_deref(), Some(QUEUE_STATE_TERMINAL));
        assert!(repaired.queue_ready_event_id.is_none());
        assert!(engine.task_get_active("seat-junior").await.unwrap().is_none());
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
