//! REST-обёртки над MCP-тулами сервера. Каждый хендлер: извлекает seat-id
//! (заголовок X-Seat-ID или кука slc_seat), вызывает MCP-тул по JSON-RPC
//! и возвращает распарсенный JSON результата.

use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;


/// Ошибка прокси: статус + текст.
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

/// Вызвать MCP-тул и вернуть распарсенный text-JSON результата.
/// Устойчив к «📬 Notifications: …»-хвосту (парсим до последней `}`).
async fn call(
    state: &Arc<AppState>,
    seat: &str,
    tool: &str,
    args: Value,
) -> Result<Value, ApiError> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args },
    });
    let resp = state
        .http
        .post(format!("{}/mcp", state.mcp_url))
        .header("Content-Type", "application/json")
        .header("X-Seat-ID", seat)
        .json(&body)
        .send()
        .await
        .map_err(|e| internal(format!("mcp request failed: {e}")))?;
    let status = resp.status();
    let jr: Value = resp
        .json()
        .await
        .map_err(|e| internal(format!("mcp response decode: {e}")))?;
    if !status.is_success() {
        let msg = jr
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("mcp error");
        return Err(ApiError(status, msg.to_string()));
    }
    if let Some(err) = jr.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("mcp error")
            .to_string();
        return Err(bad(msg));
    }
    let text = jr
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    // Возможен хвост уведомлений после JSON — берём до последней '}'.
    let end = text.rfind('}').unwrap_or(text.len().saturating_sub(1));
    serde_json::from_str(&text[..=end])
        .map_err(|e| internal(format!("mcp tool '{tool}' returned non-JSON: {e}")))
}

/// Ответ с возможным Set-Cookie для нового сида.
fn with_seat_cookie(resp: Json<Value>, cookie: Option<String>) -> Response {
    let mut r = resp.into_response();
    if let Some(c) = cookie {
        if let Ok(h) = axum::http::HeaderValue::from_str(&c) {
            r.headers_mut().insert(axum::http::header::SET_COOKIE, h);
        }
    }
    r
}

/// Общий префикс хендлеров: резолв сида + вызов тула.
macro_rules! tool_handler {
    ($name:ident, $tool:literal, $args:expr) => {
        pub async fn $name(
            State(state): State<Arc<AppState>>,
            headers: HeaderMap,
        ) -> Response {
            let (seat, cookie) = resolve_seat(&headers);
            match call(&state, &seat, $tool, $args).await {
                Ok(v) => with_seat_cookie(Json(v), cookie),
                Err(e) => e.into_response(),
            }
        }
    };
}

// ── health / stats / context ──────────────────────────────────────────

pub async fn health(State(state): State<Arc<AppState>>) -> ApiResult {
    let resp = state
        .http
        .get(format!("{}/health", state.mcp_url))
        .send()
        .await
        .map_err(|e| internal(format!("mcp health failed: {e}")))?;
    let j: Value = resp
        .json()
        .await
        .map_err(|e| internal(format!("health decode: {e}")))?;
    Ok(Json(j))
}

tool_handler!(stats, "document_stats", json!({}));
tool_handler!(context, "update_context", json!({}));
tool_handler!(list_seats, "list_seats", json!({}));
tool_handler!(notification_list, "notification_list", json!({}));
tool_handler!(focus_list, "focus_list", json!({}));

// ── documents ─────────────────────────────────────────────────────────

pub async fn list_documents(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let args = json!({
        "category": q.get("category"),
        "folder": q.get("folder"),
        "query": q.get("query"),
        "limit": q.get("limit").and_then(|v| v.parse::<u64>().ok()),
    });
    match call(&state, &seat, "list_documents", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn get_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match call(&state, &seat, "get_document", json!({"document_id": id})).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn add_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let id = body.get("document_id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        return bad("document_id required").into_response();
    }
    let args = json!({
        "document_id": id,
        "category": body.get("category").and_then(|v| v.as_str()).unwrap_or("custom"),
        "content": body.get("content").and_then(|v| v.as_str()).unwrap_or(""),
        "folder": body.get("folder").and_then(|v| v.as_str()),
    });
    match call(&state, &seat, "add_document", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn update_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let mut args = body.clone();
    args["document_id"] = json!(id);
    match call(&state, &seat, "update_document", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn delete_document(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let args = json!({
        "document_id": id,
        "purge": q.get("purge").map(|v| v == "true").unwrap_or(false),
    });
    match call(&state, &seat, "delete_document", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
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
    let args = json!({
        "query": query,
        "limit": q.get("limit").and_then(|v| v.parse::<u64>().ok()).unwrap_or(10),
    });
    match call(&state, &seat, "search", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

// ── tasks ─────────────────────────────────────────────────────────────

pub async fn list_tasks(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let args = json!({
        "project_id": q.get("project_id"),
        "status": q.get("status"),
        "limit": q.get("limit").and_then(|v| v.parse::<u64>().ok()).unwrap_or(100),
    });
    match call(&state, &seat, "list_tasks", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
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
    let args = json!({
        "name": name,
        "description": body.get("description").and_then(|v| v.as_str()).unwrap_or(""),
        "project_id": body.get("project_id").and_then(|v| v.as_str()),
        "auto_load": body.get("auto_load").cloned().unwrap_or(json!([])),
        "metadata": body.get("metadata").cloned().unwrap_or(json!({})),
    });
    match call(&state, &seat, "create_task", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn update_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let mut args = body.clone();
    args["task_id"] = json!(id);
    match call(&state, &seat, "update_task", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn delete_task(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match call(&state, &seat, "delete_task", json!({"task_id": id})).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

// ── projects ──────────────────────────────────────────────────────────

pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let args = json!({
        "status": q.get("status"),
        "limit": q.get("limit").and_then(|v| v.parse::<u64>().ok()).unwrap_or(100),
    });
    match call(&state, &seat, "list_projects", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
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
    let args = json!({
        "name": name,
        "description": body.get("description").and_then(|v| v.as_str()).unwrap_or(""),
        "auto_load": body.get("auto_load").cloned().unwrap_or(json!([])),
        "metadata": body.get("metadata").cloned().unwrap_or(json!({})),
    });
    match call(&state, &seat, "create_project", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn update_project(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let mut args = body.clone();
    args["project_id"] = json!(id);
    match call(&state, &seat, "update_project", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn delete_project(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match call(&state, &seat, "delete_project", json!({"project_id": id})).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

// ── notifications / reminders / focuses ───────────────────────────────

pub async fn pop_notifications(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match call(&state, &seat, "pop_notifications", json!({})).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn reminder_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match call(&state, &seat, "reminder_list", json!({})).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn reminder_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let remind_at = body.get("remind_at").and_then(|v| v.as_str()).unwrap_or("");
    if content.is_empty() || remind_at.is_empty() {
        return bad("content and remind_at required").into_response();
    }
    let args = json!({
        "content": content,
        "remind_at": remind_at,
        "mind_type": body.get("mind_type").and_then(|v| v.as_str()),
    });
    match call(&state, &seat, "reminder_create", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn reminder_cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match call(&state, &seat, "reminder_cancel", json!({"reminder_id": id})).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

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
    let args = json!({
        "title": title,
        "description": body.get("description").and_then(|v| v.as_str()).unwrap_or(""),
        "priority": body.get("priority").and_then(|v| v.as_u64()).unwrap_or(5),
        "mind_type": body.get("mind_type").and_then(|v| v.as_str()),
        "depends_on": body.get("depends_on").cloned().unwrap_or(json!([])),
    });
    match call(&state, &seat, "focus_add", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn focus_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let mut args = body.clone();
    args["focus_id"] = json!(id);
    match call(&state, &seat, "focus_update", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

pub async fn focus_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    match call(&state, &seat, "focus_remove", json!({"focus_id": id})).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

// ── pagination / SSE ──────────────────────────────────────────────────

pub async fn get_page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (seat, cookie) = resolve_seat(&headers);
    let response_id = q.get("response_id").cloned().unwrap_or_default();
    let page = q.get("page").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1);
    if response_id.is_empty() {
        return bad("response_id required").into_response();
    }
    let args = json!({"response_id": response_id, "page": page});
    match call(&state, &seat, "get_page", args).await {
        Ok(v) => with_seat_cookie(Json(v), cookie),
        Err(e) => e.into_response(),
    }
}

/// SSE-прокси: /api/events?seat=X → MCP /sse (уведомления, keep-alive).
pub async fn sse_events(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let (seat, _cookie) = resolve_seat(&headers);
    let url = format!("{}/sse?seat={}", state.mcp_url, seat);
    let resp = match state
        .http
        .get(&url)
        .header("X-Seat-ID", &seat)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(format!("sse upstream failed: {e}")).into_response(),
    };
    if !resp.status().is_success() {
        return ApiError(resp.status(), "sse upstream error".into()).into_response();
    }
    // Прокидываем upstream-поток как есть (text/event-stream).
    let mut builder = axum::response::Response::builder()
        .status(resp.status())
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache");
    if let Some(h) = resp.headers().get("x-accel-buffering") {
        builder = builder.header("x-accel-buffering", h);
    }
    let body = axum::body::Body::from_stream(resp.bytes_stream());
    builder
        .body(body)
        .unwrap_or_else(|_| internal("sse body error").into_response())
}
