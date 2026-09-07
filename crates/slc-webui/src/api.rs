//! REST-хендлеры веб-морды: напрямую через встроенный slc-core движок
//! (не прокси над MCP). Vault открывается этим же процессом; multi-process
//! синхронизация — через периодический `refresh` (SLC_VAULT_REFRESH_SECS).

use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{Value, json};
use slc_core::{DocFilter, DocSort, DocumentCategory, SlcEngine, SortDir};
use std::collections::HashMap;
use std::sync::Arc;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error": self.1, "code": self.0.as_u16()})),
        )
            .into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

fn internal(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg.into())
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn forbidden(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, msg.into())
}

/// Seat-id: заголовок X-Seat-ID → кука slc_seat → сгенерированный
/// (при генерации ставим Set-Cookie, чтобы UI держал тот же сид).
fn resolve_seat(headers: &HeaderMap) -> (String, Option<String>) {
    if let Some(v) = headers.get("x-seat-id").and_then(|v| v.to_str().ok()) {
        if !v.trim().is_empty() {
            return (v.trim().to_string(), None);
        }
    }
    if let Some(cookie) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("slc_seat=") {
                if !v.is_empty() {
                    return (v.to_string(), None);
                }
            }
        }
    }
    let id = format!("slc_web_{}", uuid_like());
    let cookie = format!("slc_seat={id}; Path=/; Max-Age=31536000; SameSite=Lax");
    (id, Some(cookie))
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rand = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ts.hash(&mut h);
        format!("{:x}", h.finish())
    };
    format!("{ts:x}{rand}")
}

fn with_seat_cookie(resp: Json<Value>, cookie: Option<String>) -> Response {
    let mut r = resp.into_response();
    if let Some(c) = cookie {
        if let Ok(h) = axum::http::HeaderValue::from_str(&c) {
            r.headers_mut().insert(axum::http::header::SET_COOKIE, h);
        }
    }
    r
}

/// Хендлер с seat-id: резолвит сид, выполняет $body (доступны `state` и
/// `seat` как ident-параметры) и отдаёт JSON + Set-Cookie при генерации.
macro_rules! seat_handler {
    ($name:ident, $state:ident, $seat:ident, $body:block) => {
        pub async fn $name(
            State($state): State<Arc<AppState>>,
            headers: HeaderMap,
        ) -> Response {
            let (seat, cookie) = resolve_seat(&headers);
            let $seat = seat.as_str();
            match (async { Ok::<Value, ApiError>($body) }).await {
                Ok(v) => with_seat_cookie(Json(v), cookie),
                Err(e) => e.into_response(),
            }
        }
    };
}

// ── health / stats / context ──────────────────────────────────────────

pub async fn health(State(state): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(json!({
        "status": if state.engine.health().await { "ok" } else { "degraded" },
        "server": "slc-webui",
        "services": { "storage": state.engine.health().await },
    })))
}

seat_handler!(stats, state, seat, {
    let engine = &state.engine;
    let mut by_cat = serde_json::Map::new();
    for cat in [
        DocumentCategory::Core,
        DocumentCategory::Module,
        DocumentCategory::Task,
        DocumentCategory::Project,
        DocumentCategory::CodeSnippet,
        DocumentCategory::Documentation,
        DocumentCategory::Skill,
        DocumentCategory::Custom,
        DocumentCategory::System,
    ] {
        let n = engine
            .store()
            .kb_count(&DocFilter {
                category: Some(cat),
                ..Default::default()
            })
            .await
            .map_err(|e| internal(e.to_string()))?;
        by_cat.insert(cat.as_str().into(), json!(n));
    }
    let total: u64 = by_cat.values().filter_map(|v| v.as_u64()).sum();
    let episodic = engine
        .store()
        .episodic_count(&DocFilter::default())
        .await
        .map_err(|e| internal(e.to_string()))?;
    let seats = engine.seats.list_active(1000).await.map_err(|e| internal(e.to_string()))?.len();
    json!({
        "total": total,
        "by_category": by_cat,
        "episodic": episodic,
        "seats": seats,
        "tasks": by_cat.get("task").cloned().unwrap_or(json!(0)),
        "projects": by_cat.get("project").cloned().unwrap_or(json!(0)),
    })
});

seat_handler!(context, state, seat, {
    let engine = &state.engine;
    let limit = engine.context_limit_for(&seat).await.map_err(|e| internal(e.to_string()))?;
    let active = engine.seats.get_active_document(&seat).await.map_err(|e| internal(e.to_string()))?;
    let projects = project_list(engine).await?;
    let tasks = task_list(engine).await?;
    let pending = engine.pending_notification_count(&seat).await.map_err(|e| internal(e.to_string()))?;
    json!({
        "seat": seat,
        "limit_tokens": limit,
        "active_document": active,
        "projects": projects,
        "tasks": tasks,
        "pending_notifications": pending,
    })
});

seat_handler!(list_seats, state, seat, {
    let seats = state
        .engine
        .seats
        .list_active(200)
        .await
        .map_err(|e| internal(e.to_string()))?;
    json!({
        "seats": seats.iter().map(|s| json!({
            "seat_id": s.seat_id,
            "name": s.name,
            "status": s.status.as_str(),
            "created_at": s.created_at.to_rfc3339(),
            "last_accessed": s.last_accessed.to_rfc3339(),
            "usage_stats": s.usage_stats,
            "active_document_id": s.active_document_id,
        })).collect::<Vec<_>>(),
        "count": seats.len(),
    })
});

// ── documents ─────────────────────────────────────────────────────────

pub async fn list_documents(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let engine = &state.engine;
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(200).min(500);
    let filter = DocFilter {
        category: q.get("category").and_then(|s| DocumentCategory::parse(s)),
        visible_to: Some(seat.clone()),
        ..Default::default()
    };
    match engine
        .store()
        .kb_find(&filter, &DocSort::by_updated(SortDir::Desc), limit)
        .await
    {
        Ok(docs) => {
            let folder = q.get("folder").filter(|s| !s.is_empty());
            let query = q.get("query").map(|s| s.to_lowercase()).filter(|s| !s.is_empty());
            let out: Vec<Value> = docs
                .iter()
                .filter(|d| folder.map_or(true, |f| d.folder.as_deref() == Some(f)))
                .filter(|d| {
                    query.as_ref().map_or(true, |qq| {
                        d.document_id.to_lowercase().contains(qq)
                            || d.content.to_lowercase().contains(qq)
                    })
                })
                .map(|d| {
                    json!({
                        "document_id": d.document_id,
                        "category": d.category.as_str(),
                        "folder": d.folder,
                        "tags": d.tags,
                        "seat_id": d.seat_id,
                        "created_at": d.created_at.to_rfc3339(),
                        "updated_at": d.updated_at.to_rfc3339(),
                        "content_preview": d.content.chars().take(200).collect::<String>(),
                    })
                })
                .collect();
            with_seat_cookie(Json(json!({"success": true, "documents": out, "count": out.len()})), cookie)
        }
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn get_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match state.engine.get_document(&id).await {
        Ok(Some(d)) => with_seat_cookie(
            Json(json!({
                "document_id": d.document_id,
                "category": d.category.as_str(),
                "folder": d.folder,
                "content": d.content,
                "tags": d.tags,
                "metadata": d.metadata,
                "seat_id": d.seat_id,
                "auto_load": d.auto_load,
                "references": d.references,
                "created_at": d.created_at.to_rfc3339(),
                "updated_at": d.updated_at.to_rfc3339(),
            })),
            cookie,
        ),
        Ok(None) => ApiError(StatusCode::NOT_FOUND, format!("not found: {id}")).into_response(),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn add_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let engine = &state.engine;
    let id = body.get("document_id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return bad("document_id required").into_response();
    }
    let cat = body
        .get("category")
        .and_then(|v| v.as_str())
        .and_then(DocumentCategory::parse)
        .unwrap_or(DocumentCategory::Custom);
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let folder = body.get("folder").and_then(|v| v.as_str()).map(String::from);
    let mut doc = slc_core::Document::with_folder(
        id,
        cat,
        folder,
        content,
        slc_core::DocMeta::default(),
        vec![],
        Some(seat.clone()),
    );
    match engine.add_document(&mut doc).await {
        Ok(()) => with_seat_cookie(
            Json(json!({
                "success": true,
                "document_id": doc.document_id,
                "folder": doc.folder.clone().unwrap_or_else(|| doc.default_folder()),
            })),
            cookie,
        ),
        Err(e) => ApiError(StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

pub async fn update_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let engine = &state.engine;
    let doc = match engine.get_document(&id).await {
        Ok(Some(d)) => d,
        Ok(None) => {
            return ApiError(StatusCode::NOT_FOUND, format!("not found: {id}")).into_response();
        }
        Err(e) => return internal(e.to_string()).into_response(),
    };
    let mut doc = doc;
    if !engine.can_write_document(&seat, &doc) {
        return forbidden(format!("seat {seat} has no right to update document {id}"))
            .into_response();
    }
    if let Some(c) = body.get("content").and_then(|v| v.as_str()) {
        doc.content = c.to_string();
        doc.content_hash = slc_core::content_hash(c);
    }
    if let Some(t) = body.get("tags").and_then(|v| v.as_array()) {
        doc.tags = t.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    }
    if let Some(al) = body.get("auto_load").and_then(|v| v.as_array()) {
        doc.auto_load = al.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    }
    if let Some(r) = body.get("references").and_then(|v| v.as_array()) {
        doc.references = r.iter().filter_map(|x| x.as_str().map(String::from)).collect();
    }
    if let Some(m) = body.get("metadata").and_then(|v| v.as_object()) {
        for (k, v) in m {
            doc.metadata.extra.insert(k.clone(), v.clone());
        }
    }
    if let Some(s) = body.get("seat_id").and_then(|v| v.as_str()) {
        if !s.is_empty() && !engine.can_manage_target(&seat, s) {
            return forbidden(format!(
                "seat {seat} has no right to assign document {id} to seat {s}"
            ))
            .into_response();
        }
        doc.seat_id = if s.is_empty() { None } else { Some(s.into()) };
    }
    doc.updated_at = chrono::Utc::now();
    doc.version += 1;
    match engine.store().kb_replace(&doc).await {
        Ok(_) => {
            let _ = engine.reembed_document(&id).await;
            with_seat_cookie(Json(json!({"success": true, "document_id": id})), cookie)
        }
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn delete_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let engine = &state.engine;
    if let Ok(Some(doc)) = engine.get_document(&id).await {
        if !engine.can_write_document(&seat, &doc) {
            return forbidden(format!("seat {seat} has no right to delete document {id}"))
                .into_response();
        }
        if slc_core::tasks::is_workflow_task(&doc) {
            return ApiError(
                StatusCode::CONFLICT,
                "workflow tasks cannot be deleted; preserve their event history".into(),
            )
            .into_response();
        }
    }
    let purge = q.get("purge").map(|v| v == "true").unwrap_or(false);
    let res = if purge {
        engine.store().kb_purge(&id).await
    } else {
        engine.store().kb_soft_delete(&id).await
    };
    let ok = match res {
        Ok(ok) => ok,
        Err(e) => return internal(e.to_string()).into_response(),
    };
    with_seat_cookie(Json(json!({"success": ok, "document_id": id, "purged": purge})), cookie)
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let query = q.get("q").cloned().unwrap_or_default();
    if query.is_empty() {
        return bad("q required").into_response();
    }
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(10);
    match state.engine.search(&query, Some(&seat), limit).await {
        Ok(hits) => with_seat_cookie(
            Json(json!({
                "results": hits.iter().map(|h| json!({
                    "document_id": h.document.document_id,
                    "category": h.document.category.as_str(),
                    "folder": h.document.folder,
                    "score": h.rank_score,
                    "content": h.document.content.chars().take(2000).collect::<String>(),
                })).collect::<Vec<_>>(),
            })),
            cookie,
        ),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

// ── tasks / projects ──────────────────────────────────────────────────

pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let project_id = q.get("project_id").filter(|s| !s.is_empty()).map(String::as_str);
    let status = q.get("status").filter(|s| !s.is_empty()).map(String::as_str);
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(200);
    match state.engine.task_list(&seat, project_id, status, limit).await {
        Ok(tasks) => with_seat_cookie(
            Json(json!({
                "success": true,
                "tasks": tasks.iter().map(|t| json!({
                    "task_id": t.task_id, "name": t.name, "status": t.status,
                    "project_id": t.project_id, "auto_load_count": t.auto_load.len(),
                })).collect::<Vec<_>>(),
                "count": tasks.len(),
            })),
            cookie,
        ),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn create_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return bad("name required").into_response();
    }
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let project_id = body.get("project_id").and_then(|v| v.as_str());
    let auto_load: Vec<String> = body
        .get("auto_load")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let metadata = body.get("metadata").cloned().unwrap_or(json!({}));
    match state
        .engine
        .task_create(&seat, name, description, project_id, &auto_load, &metadata)
        .await
    {
        Ok(t) => with_seat_cookie(
            Json(json!({
                "success": true, "task_id": t.task_id, "name": t.name,
                "project_id": t.project_id, "message": format!("Task '{}' created", t.name),
            })),
            cookie,
        ),
        Err(e) => ApiError(StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

pub async fn update_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let name = body.get("name").and_then(|v| v.as_str());
    let description = body.get("description").and_then(|v| v.as_str());
    let description_patch = body.get("description_patch").cloned();
    let project_id = body
        .get("project_id")
        .and_then(|v| v.as_str())
        .map(|p| if p.is_empty() { None } else { Some(p) });
    let auto_load = body
        .get("auto_load")
        .map(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
                .unwrap_or_default()
        });
    let status = body.get("status").and_then(|v| v.as_str());
    let metadata = body.get("metadata").cloned();
    match state
        .engine
        .task_update(
            &seat,
            &id,
            name,
            description,
            description_patch.as_ref(),
            project_id,
            auto_load.as_deref(),
            status,
            metadata.as_ref(),
        )
        .await
    {
        Ok(Some(_)) => {
            let _ = state.engine.reembed_document(&id).await;
            with_seat_cookie(Json(json!({"success": true, "task_id": id})), cookie)
        }
        Ok(None) => ApiError(StatusCode::NOT_FOUND, format!("task not found: {id}")).into_response(),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn delete_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match state.engine.task_delete(&seat, &id).await {
        Ok(ok) => with_seat_cookie(
            Json(json!({"success": ok, "task_id": id, "message": if ok { "Task deleted" } else { "Task not found" }})),
            cookie,
        ),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let limit = q.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(200);
    match state.engine.project_list(&seat, None, limit).await {
        Ok(projects) => with_seat_cookie(
            Json(json!({
                "success": true,
                "projects": projects.iter().map(|p| json!({
                    "project_id": p.project_id, "name": p.name, "status": p.status,
                    "auto_load_count": p.auto_load.len(),
                })).collect::<Vec<_>>(),
                "count": projects.len(),
            })),
            cookie,
        ),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn create_project(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        return bad("name required").into_response();
    }
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let auto_load: Vec<String> = body
        .get("auto_load")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let metadata = body.get("metadata").cloned().unwrap_or(json!({}));
    match state
        .engine
        .project_create(&seat, name, description, &auto_load, &metadata)
        .await
    {
        Ok(p) => with_seat_cookie(
            Json(json!({
                "success": true, "project_id": p.project_id, "name": p.name,
                "message": format!("Project '{}' created", p.name),
            })),
            cookie,
        ),
        Err(e) => ApiError(StatusCode::CONFLICT, e.to_string()).into_response(),
    }
}

pub async fn update_project(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let name = body.get("name").and_then(|v| v.as_str());
    let description = body.get("description").and_then(|v| v.as_str());
    let description_patch = body.get("description_patch").cloned();
    let auto_load = body
        .get("auto_load")
        .map(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
                .unwrap_or_default()
        });
    let status = body.get("status").and_then(|v| v.as_str());
    let metadata = body.get("metadata").cloned();
    match state
        .engine
        .project_update(
            &seat,
            &id,
            name,
            description,
            description_patch.as_ref(),
            auto_load.as_deref(),
            status,
            metadata.as_ref(),
        )
        .await
    {
        Ok(Some(_)) => {
            let _ = state.engine.reembed_document(&id).await;
            with_seat_cookie(Json(json!({"success": true, "project_id": id})), cookie)
        }
        Ok(None) => ApiError(StatusCode::NOT_FOUND, format!("project not found: {id}")).into_response(),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn delete_project(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match state.engine.project_delete(&seat, &id).await {
        Ok(ok) => with_seat_cookie(
            Json(json!({"success": ok, "project_id": id, "message": if ok { "Project deleted" } else { "Project not found" }})),
            cookie,
        ),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

// ── notifications / reminders / focuses ───────────────────────────────

seat_handler!(notification_list, state, seat, {
    let notes = state
        .engine
        .list_notifications(&seat, None)
        .await
        .map_err(|e| internal(e.to_string()))?;
    json!({
        "notifications": notes.iter().map(|n| json!({
            "notification_id": n.notification_id,
            "source": n.source,
            "title": n.title,
            "body": n.body,
            "status": n.status,
            "created_at": n.created_at.to_rfc3339(),
        })).collect::<Vec<_>>(),
    })
});

seat_handler!(pop_notifications, state, seat, {
    let notes = state
        .engine
        .pop_notifications(&seat, 10)
        .await
        .map_err(|e| internal(e.to_string()))?;
    json!({
        "notifications": notes.iter().map(|n| json!({
            "notification_id": n.notification_id,
            "source": n.source,
            "title": n.title,
            "body": n.body,
        })).collect::<Vec<_>>(),
        "count": notes.len(),
    })
});

seat_handler!(reminder_list, state, seat, {
    let reminders = state.engine.reminder_list(&seat).await.map_err(|e| internal(e.to_string()))?;
    json!({
        "reminders": reminders.iter().map(|r| json!({
            "reminder_id": r.reminder_id,
            "content": r.content,
            "status": r.status,
            "remind_at": r.remind_at.to_rfc3339(),
        })).collect::<Vec<_>>(),
    })
});

pub async fn reminder_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let remind_at_raw = body.get("remind_at").and_then(|v| v.as_str()).unwrap_or("");
    let remind_at = match chrono::DateTime::parse_from_rfc3339(remind_at_raw) {
        Ok(d) => d.with_timezone(&chrono::Utc),
        Err(_) => {
            return bad("remind_at must be RFC3339 (e.g. 2026-08-20T15:00:00Z)").into_response();
        }
    };
    let mind_type = body.get("mind_type").and_then(|v| v.as_str());
    match state
        .engine
        .reminder_create(&seat, content, remind_at, mind_type)
        .await
    {
        Ok(r) => with_seat_cookie(
            Json(json!({"success": true, "reminder_id": r.reminder_id, "remind_at": r.remind_at.to_rfc3339()})),
            cookie,
        ),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn reminder_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match state.engine.reminder_cancel(&seat, &id).await {
        Ok(ok) => with_seat_cookie(Json(json!({"success": ok, "reminder_id": id})), cookie),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

seat_handler!(focus_list, state, seat, {
    let focuses = state.engine.focus_list(&seat, None).await.map_err(|e| internal(e.to_string()))?;
    json!({
        "focuses": focuses.iter().map(|f| json!({
            "focus_id": f.focus_id,
            "title": f.title,
            "description": f.description,
            "priority": f.priority,
            "depends_on": f.depends_on,
        })).collect::<Vec<_>>(),
    })
});

pub async fn focus_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");
    if title.is_empty() {
        return bad("title required").into_response();
    }
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("");
    let priority = body.get("priority").and_then(|v| v.as_i64()).unwrap_or(5);
    let depends_on: Vec<String> = body
        .get("depends_on")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mind_type = body.get("mind_type").and_then(|v| v.as_str());
    match state
        .engine
        .focus_add(&seat, title, description, priority, &depends_on, mind_type)
        .await
    {
        Ok(f) => with_seat_cookie(
            Json(json!({"success": true, "focus_id": f.focus_id, "priority": f.priority})),
            cookie,
        ),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn focus_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let title = body.get("title").and_then(|v| v.as_str());
    let description = body.get("description").and_then(|v| v.as_str());
    let priority = body.get("priority").and_then(|v| v.as_i64());
    let depends_on = body
        .get("depends_on")
        .map(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
                .unwrap_or_default()
        });
    match state
        .engine
        .focus_update(&seat, &id, title, description, priority, depends_on.as_deref())
        .await
    {
        Ok(ok) => with_seat_cookie(Json(json!({"success": ok, "focus_id": id})), cookie),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn focus_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match state.engine.focus_remove(&seat, &id).await {
        Ok(ok) => with_seat_cookie(Json(json!({"success": ok, "focus_id": id})), cookie),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

// ── SSE: лента уведомлений (поллинг каждые 5с, keep-alive) ────────────

pub async fn sse_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let (seat, _cookie) = resolve_seat(&headers);
    let engine = state.engine.clone();
    let stream = async_stream::stream! {
        let mut last_count = 0usize;
        loop {
            let count = engine
                .pending_notification_count(&seat)
                .await
                .unwrap_or(last_count);
            if count != last_count {
                let notes = engine.list_notifications(&seat, Some("pending")).await.unwrap_or_default();
                let payload = serde_json::to_string(&notes.iter().map(|n| serde_json::json!({
                    "notification_id": n.notification_id,
                    "source": n.source,
                    "title": n.title,
                    "body": n.body,
                })).collect::<Vec<_>>()).unwrap_or_default();
                yield Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default()
                        .event("notification")
                        .data(payload),
                );
                last_count = count;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    };
    axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

// ── helpers ───────────────────────────────────────────────────────────

async fn project_list(engine: &SlcEngine) -> Result<Vec<Value>, ApiError> {
    let projects = engine
        .store()
        .kb_find(
            &DocFilter {
                category: Some(DocumentCategory::Project),
                ..Default::default()
            },
            &DocSort::by_created(SortDir::Asc),
            100,
        )
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(projects
        .iter()
        .map(|p| {
            json!({
                "document_id": p.document_id,
                "name": p.metadata.extra.get("name").and_then(|v| v.as_str()).unwrap_or(&p.document_id),
                "updated_at": p.updated_at.to_rfc3339(),
            })
        })
        .collect())
}

async fn task_list(engine: &SlcEngine) -> Result<Vec<Value>, ApiError> {
    let tasks = engine
        .store()
        .kb_find(
            &DocFilter {
                category: Some(DocumentCategory::Task),
                ..Default::default()
            },
            &DocSort::by_updated(SortDir::Desc),
            100,
        )
        .await
        .map_err(|e| internal(e.to_string()))?;
    Ok(tasks
        .iter()
        .map(|t| {
            let mut v = json!({
                "document_id": t.document_id,
                "project": t.project_slug(),
                "updated_at": t.updated_at.to_rfc3339(),
            });
            if let Some(st) = t.metadata.extra.get("status").and_then(|x| x.as_str()) {
                v["status"] = json!(st);
            }
            v
        })
        .collect())
}
