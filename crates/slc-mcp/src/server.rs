//! MCP server: JSON-RPC 2.0 over HTTP (`POST /mcp`) + streamable-HTTP SSE
//! (`GET /sse` → `POST /messages`) + `GET /health`. Tools are the canonical
//! snake_case catalog. Auth modes: `legacy_seat_id` (default),
//! `bearer_plus_seat`, `embedded` (via `SLC_MCP_AUTH`).

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderName, HeaderValue, StatusCode},
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, post, put},
};
use serde_json::{Value, json};
use slc_core::{
    DocFilter, DocMeta, DocSort, Document, DocumentCategory, SlcEngine, SortDir, TaskListScope,
};
use std::sync::Arc;

use crate::{auth, webui};

pub struct AppState {
    pub engine: Arc<SlcEngine>,
    /// Server→client notification fan-out keyed by seat id.
    pub events: tokio::sync::broadcast::Sender<Value>,
    /// Pending MCP sampling requests (id → answer channel); the sampling
    /// LlmClient registers here and the client's response resolves it.
    pub sampling: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<String>>>,
    >,
    /// Каталог со статикой SPA (web-ui/dist) для встроенной веб-морды.
    pub dist: std::path::PathBuf,
    /// Авторизация веб-морды (users/JWT/RBAC/Yandex OAuth, SLC_AUTH).
    pub auth: std::sync::Arc<auth::AuthState>,
}

pub async fn run(
    engine: SlcEngine,
    port: u16,
    dist: std::path::PathBuf,
    auth_state: auth::AuthState,
    sampling_out: Option<tokio::sync::mpsc::Receiver<Value>>,
    sampling: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<String>>>,
    >,
) -> anyhow::Result<()> {
    if let Err(e) = engine.start_background().await {
        tracing::warn!("failed to start background timers: {e}");
    }
    let (tx, _) = tokio::sync::broadcast::channel(256);
    // Sampling requests from the LlmClient → forwarded to the client's SSE.
    if let Some(mut rx) = sampling_out {
        let out_tx = tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let _ = out_tx.send(msg);
            }
        });
    }
    let state = Arc::new(AppState {
        engine: Arc::new(engine),
        events: tx,
        sampling,
        dist,
        auth: std::sync::Arc::new(auth_state),
    });
    let app = Router::new()
        // Streamable-HTTP клиенты (go-sdk / Yandex AI Studio) открывают
        // SSE-поток GET-ом на URL сервера — отдаём тот же стрим, что и /sse.
        .route(
            "/mcp",
            get(sse_endpoint).post(mcp).delete(mcp_session_delete),
        )
        .route("/sse", get(sse_endpoint))
        .route("/messages", post(messages))
        .route("/health", get(health))
        // ── Веб-морда (REST + SPA, тот же процесс) ──
        .route("/api/health", get(webui::api::health))
        .route("/api/stats", get(webui::api::stats))
        .route("/api/context", get(webui::api::context))
        .route(
            "/api/documents",
            get(webui::api::list_documents).post(webui::api::add_document),
        )
        .route(
            "/api/documents/{id}",
            get(webui::api::get_document)
                .put(webui::api::update_document)
                .delete(webui::api::delete_document),
        )
        .route("/api/search", get(webui::api::search))
        .route(
            "/api/tasks",
            get(webui::api::list_tasks).post(webui::api::create_task),
        )
        .route(
            "/api/tasks/{id}",
            put(webui::api::update_task).delete(webui::api::delete_task),
        )
        .route(
            "/api/projects",
            get(webui::api::list_projects).post(webui::api::create_project),
        )
        .route(
            "/api/projects/{id}",
            put(webui::api::update_project).delete(webui::api::delete_project),
        )
        .route("/api/seats", get(webui::api::list_seats))
        .route(
            "/api/notifications",
            get(webui::api::notification_list).post(webui::api::pop_notifications),
        )
        .route(
            "/api/reminders",
            get(webui::api::reminder_list).post(webui::api::reminder_create),
        )
        .route("/api/reminders/{id}", delete(webui::api::reminder_cancel))
        .route(
            "/api/focuses",
            get(webui::api::focus_list).post(webui::api::focus_add),
        )
        .route(
            "/api/focuses/{id}",
            put(webui::api::focus_update).delete(webui::api::focus_remove),
        )
        .route("/api/events", get(webui::api::sse_events))
        // ── Авторизация веб-морды (полный порт легаси) ──
        .route("/api/auth/register", post(auth::routes::register))
        .route("/api/auth/login", post(auth::routes::login))
        .route("/api/auth/refresh", post(auth::routes::refresh))
        .route("/api/auth/logout", post(auth::routes::logout))
        .route("/api/auth/me", get(auth::routes::me))
        .route(
            "/api/auth/oauth/yandex",
            get(auth::routes::oauth_yandex_redirect).post(auth::routes::oauth_yandex_code),
        )
        .route(
            "/api/auth/oauth/yandex/callback",
            get(auth::routes::oauth_yandex_callback),
        )
        .route(
            "/api/auth/oauth/yandex/callback_uri",
            get(auth::routes::oauth_callback_uri),
        )
        .route("/api/auth/exchange", post(auth::routes::exchange))
        .route("/api/auth/users", get(auth::routes::list_users))
        .route(
            "/api/auth/users/{user_id}/groups",
            put(auth::routes::update_user_groups),
        )
        .route(
            "/api/auth/users/{user_id}/active",
            put(auth::routes::toggle_user_active),
        )
        .route("/api/auth/groups", get(auth::routes::list_groups))
        .route(
            "/api/admin/oauth/rules",
            get(auth::routes::oauth_rules_list).post(auth::routes::oauth_rule_create),
        )
        .route(
            "/api/admin/oauth/rules/{rule_id}",
            put(auth::routes::oauth_rule_update).delete(auth::routes::oauth_rule_delete),
        )
        .route(
            "/api/admin/oauth/ya360/status",
            get(auth::routes::ya360_status),
        )
        .fallback(webui::static_files::handler)
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(
        "SLC MCP listening on http://{addr}/mcp (SSE: /sse → /messages; web UI: /api, /)"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("SLC MCP stopped gracefully");
    Ok(())
}

/// Wait for Ctrl-C or SIGTERM (docker stop) — clean shutdown so SSE clients
/// see a proper close and reconnect instead of a dropped connection.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": if state.engine.health().await { "ok" } else { "degraded" },
        "server": concat!("slc-mcp ", env!("CARGO_PKG_VERSION")),
        "services": { "storage": state.engine.health().await },
    }))
}

/// Streamable-HTTP SSE endpoint. Emits an `endpoint` event pointing the
/// client at `/messages`, then streams server→client notifications for the
/// seat supplied via `X-Seat-ID` (or `?seat=`).
///
/// Authentication: in `bearer_plus_seat` mode a valid `Authorization: Bearer`
/// is required (same rule as `/mcp`); otherwise any non-empty seat is
/// accepted (legacy behaviour).
async fn sse_endpoint(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let seat = seat_from_request(&headers)
        .or_else(|| params.get("seat").cloned())
        .unwrap_or_default();
    let mode = slc_core::auth_mode_from_env();
    let bearer = bearer_from_request(&headers);
    if mode == slc_core::AuthMode::BearerPlusSeat {
        match slc_core::authenticate(mode, Some(&seat), bearer.as_deref()) {
            Ok(Some(_)) => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "missing/invalid Authorization: Bearer token"})),
                )
                    .into_response();
            }
        }
    }
    let rx = state.events.subscribe();

    let stream: std::pin::Pin<
        Box<dyn futures::stream::Stream<Item = Result<Event, std::convert::Infallible>> + Send>,
    > = Box::pin(async_stream::stream! {
        // Tell the client where to POST JSON-RPC.
        yield Ok(Event::default()
            .event("endpoint")
            .data("/messages")
            .id("endpoint"));
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(evt) => {
                    // Only forward events for this seat (deny-by-default —
                    // see seat_matches_event).
                    let seat_matches = seat_matches_event(&evt, &seat);
                    if seat_matches {
                        // Standard JSON-RPC traffic (legacy replies and
                        // server-initiated sampling requests) uses an MCP
                        // `message` event with the raw JSON-RPC envelope.
                        // Internal lifecycle events retain the custom
                        // `notification` event and wrapper metadata.
                        if let Some((event_name, payload)) = sse_delivery(&evt) {
                            yield Ok(Event::default().event(event_name).data(payload.to_string()));
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}

/// Streamable-HTTP messages endpoint — accepts JSON-RPC and returns the
/// response inline (matching the MCP Streamable HTTP spec for single
/// responses). The optional `sessionId` header is echoed back.
async fn messages(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<Value>,
) -> axum::response::Response {
    // JSON-RPC notifications never receive a JSON-RPC response.  Streamable
    // HTTP represents successful notification acceptance as HTTP 202 with an
    // empty body (MCP 2025-03-26, Transports).  This path is shared with the
    // legacy SSE transport so standard SDKs do not receive a fabricated
    // response with `id: null` on their /messages POST.
    if is_jsonrpc_notification(&req) {
        return StatusCode::ACCEPTED.into_response();
    }
    let sid = headers
        .get("sessionId")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let seat = seat_from_request(&headers).unwrap_or_default();
    let (status, Json(body)) = mcp_request(State(state.clone()), headers, Json(req)).await;
    // Ответ доставляется ДВУМЯ путями сразу:
    // 1. SSE-событие `message` на потоке /sse — легаси-SSE клиенты (SDK
    //    SSEClientTransport) игнорируют тело POST и ждут ответ там;
    // 2. inline-тело — streamable-HTTP клиенты и гибридные клиенты, которые
    //    читают ответ из тела.
    // Лишний канал просто игнорируется клиентом, чей pending уже resolved.
    if body.get("id").is_some() {
        let _ = state.events.send(json!({
            "type": "rpc_response",
            "seat_id": seat,
            "response": body,
        }));
    }
    if let Some(sid) = sid {
        if body.get("result").is_some() {
            let mut b = body;
            if let Some(obj) = b.as_object_mut() {
                obj.insert("_sessionId".into(), Value::String(sid));
            }
            (status, Json(b)).into_response()
        } else {
            (status, Json(body)).into_response()
        }
    } else {
        (status, Json(body)).into_response()
    }
}

/// Resolve the seat from the `X-Seat-ID` header (auto-provision on miss).
fn seat_from_request(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-seat-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the bearer token from the `Authorization: Bearer …` header.
fn bearer_from_request(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaginationPolicy {
    enabled: bool,
    page_token_limit: Option<usize>,
    context_token_limit: Option<usize>,
}

/// Resolve the response-shaping policy for one MCP connection. Custom HTTP
/// headers are transport metadata, not protocol changes: clients that do not
/// send them continue to receive the server defaults.
fn pagination_policy_from_request(headers: &axum::http::HeaderMap) -> PaginationPolicy {
    let enabled = headers
        .get("x-slc-pagination")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_pagination_enabled)
        .unwrap_or_else(slc_core::pagination::pagination_enabled_from_env);
    let page_token_limit = headers
        .get("x-slc-page-token-limit")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(slc_core::pagination::normalize_page_token_limit);
    let context_token_limit = headers
        .get("x-slc-context-token-limit")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0);
    PaginationPolicy {
        enabled,
        page_token_limit,
        context_token_limit,
    }
}

fn parse_pagination_enabled(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

fn effective_context_token_limit(connection_limit: Option<usize>, seat_limit: usize) -> usize {
    connection_limit.unwrap_or(seat_limit)
}

#[axum::debug_handler]
async fn mcp(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<Value>,
) -> axum::response::Response {
    // Notifications are intentionally accepted before seat authentication:
    // initialize/list/ping are likewise connection-level operations, and an
    // `initialized` notification carries no tool data.  Unknown notifications
    // are ignored as required by JSON-RPC rather than answered with an error.
    if is_jsonrpc_notification(&req) {
        return StatusCode::ACCEPTED.into_response();
    }

    let initialize = is_initialize_request(&req);
    let response = mcp_request(State(state), headers, Json(req)).await;
    let mut response = response.into_response();
    if initialize && response.status().is_success() {
        // Streamable HTTP clients only open their long-lived GET channel for
        // server-initiated requests after the initialization response assigns
        // a session. Without this header tools/call still works, but MCP
        // sampling deadlocks: SLC waits for createMessage while the client has
        // no receive stream. The server remains otherwise stateless; auth and
        // seat isolation are enforced independently on every GET/POST.
        let session_id = uuid::Uuid::new_v4().to_string();
        response.headers_mut().insert(
            HeaderName::from_static("mcp-session-id"),
            HeaderValue::from_str(&session_id).expect("UUID is a valid header value"),
        );
    }
    response
}

/// SLC keeps no transport session state, so termination is an idempotent
/// acknowledgement. Supporting DELETE prevents conforming clients from
/// reporting a spurious 405 when their Streamable HTTP session closes.
async fn mcp_session_delete() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn mcp_request(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    // A response to one of our sampling requests (client answered via
    // POST /messages): resolve the pending channel and reply with nothing.
    // Runs AFTER authentication (any unauthenticated client who learned a
    // request_id from SSE must not be able to inject text into the
    // compression pipeline — that would poison the agent's memory).
    if method.is_empty() && id.is_some() {
        if let Some(rid) = id.as_ref().and_then(|v| v.as_str()) {
            if let Some(tx) = state.sampling.lock().unwrap().remove(rid) {
                let text = sampling_response_text(&req).unwrap_or_default();
                let _ = tx.try_send(text);
                return (StatusCode::OK, Json(json!({})));
            }
        }
    }

    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let engine = state.engine.as_ref();
    let seat_hdr = seat_from_request(&headers);
    let bearer = bearer_from_request(&headers);
    let pagination = pagination_policy_from_request(&headers);

    // Tool calls need a seat; initialize/list/ping are unauthenticated.
    if !matches!(
        method,
        "initialize" | "tools/list" | "prompts/list" | "prompts/get" | "ping"
    ) {
        let mode = slc_core::auth_mode_from_env();
        match slc_core::authenticate(mode, seat_hdr.as_deref(), bearer.as_deref()) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"missing/invalid X-Seat-ID header"}}),
                    ),
                );
            }
            Err(e) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":e.to_string()}}),
                    ),
                );
            }
        }
    }

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-03-26",
            "capabilities": { "tools": {}, "prompts": {} },
            "serverInfo": { "name": "slc-mcp", "version": env!("CARGO_PKG_VERSION") },
        })),
        // MCP ping responses must contain an object result.  Returning JSON
        // null is legal in generic JSON-RPC, but the MCP SDK models `result`
        // as an object and rejects null before the keepalive can complete.
        "ping" => Ok(ping_result()),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "prompts/list" => Ok(json!({ "prompts": prompts() })),
        "prompts/get" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            match name {
                "instructions" => Ok(json!({
                    "name": "instructions",
                    "description": "Complete agent working contract: lifecycle, document maintenance, auto_load versus references, and reflection over history",
                    "arguments": [],
                    "messages": [ { "role": "user", "content": { "type": "text", "text": INSTRUCTIONS_PROMPT } } ],
                })),
                "check_notifications" => Ok(json!({
                    "name": "check_notifications",
                    "description": "Pop pending notifications for this seat and report them to the user",
                    "arguments": [{ "name": "limit", "description": "max notifications (default 5)", "required": false }],
                })),
                _ => Err(json!({"code": -32602, "message": format!("unknown prompt: {name}")})),
            }
        }
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let seat = seat_hdr.clone().unwrap_or_default();
            call_tool(engine, &seat, name, &args, &state.events, pagination).await
        }
        _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };

    match result {
        Ok(result) => (
            StatusCode::OK,
            Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
        ),
        Err(error) => (
            StatusCode::OK,
            Json(json!({"jsonrpc":"2.0","id":id,"error":error})),
        ),
    }
}

fn is_jsonrpc_notification(req: &Value) -> bool {
    req.get("id").is_none() && req.get("method").and_then(Value::as_str).is_some()
}

fn is_initialize_request(req: &Value) -> bool {
    req.get("method").and_then(Value::as_str) == Some("initialize") && req.get("id").is_some()
}

/// Extract text from a standard MCP `CreateMessageResult`. Current MCP uses a
/// single content block; accept the older array representation as well so a
/// rolling client upgrade cannot turn successful sampling into an empty
/// summary.
fn sampling_response_text(response: &Value) -> Option<String> {
    let content = response.pointer("/result/content")?;
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    content
        .as_array()?
        .iter()
        .find_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Hybrid search over the knowledge base (semantic + BM25). ВЫХОД ПАГИНИРУЕТСЯ — при _pagination дочитай все страницы.",
            "inputSchema": {"type":"object","properties":{
                "query": {"type":"string","description":"search query"},
                "limit": {"type":"number","default":10}
            },"required":["query"]}
        }),
        json!({
            "name": "get_document",
            "description": "Load a document by its unique name id. ВЫХОД ПАГИНИРУЕТСЯ для больших документов: при _pagination дочитай ВСЕ страницы/части (part k/n) — только так получишь полный контент.",
            "inputSchema": {"type":"object","properties":{
                "document_id": {"type":"string"}
            },"required":["document_id"]}
        }),
        json!({
            "name": "add_document",
            "description": "Add a knowledge document (projects/tasks/docs are all documents)",
            "inputSchema": {"type":"object","properties":{
                "document_id": {"type":"string"},
                "category": {"type":"string","enum":["core","module","task","project","code_snippet","documentation","skill","custom","system"]},
                "content": {"type":"string"},
                "folder": {"type":"string","description":"vault folder, e.g. projects/vassista"}
            },"required":["document_id","category","content"]}
        }),
        json!({
            "name": "remember",
            "description": "Record an episodic event (L1) — the diary entry, NOT part of the RAG store",
            "inputSchema": {"type":"object","properties":{
                "event_id": {"type":"string"},
                "content": {"type":"string"}
            },"required":["event_id","content"]}
        }),
        json!({
            "name": "recall",
            "description": "Recent episodic history for the seat (separate from KB search)",
            "inputSchema": {"type":"object","properties":{
                "limit": {"type":"number","default":10}
            },"required":[]}
        }),
        json!({
            "name": "seat_info",
            "description": "Current seat info + usage stats",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "compress_now",
            "description": "Run progressive summarization L1→L4 for the seat",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "consolidate_now",
            "description": "Extract learned facts from episodic memory",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "focus_add",
            "description": "Add a focus item (something the user is concentrating on)",
            "inputSchema": {"type":"object","properties":{
                "title": {"type":"string"},
                "description": {"type":"string","default":""},
                "priority": {"type":"number","minimum":1,"maximum":10,"default":5},
                "depends_on": {"type":"array","items":{"type":"string"},"description":"focus ids this depends on"},
                "mind_type": {"type":"string","enum":["front","planner","executor","critic","shared"]}
            },"required":["title"]}
        }),
        json!({
            "name": "focus_list",
            "description": "List active focus items for the seat. An operator may select an explicitly allowed subordinate with target_seat.",
            "inputSchema": {"type":"object","properties":{
                "mind_type": {"type":"string","enum":["front","planner","executor","critic","shared"]},
                "target_seat": {"type":"string","description":"current seat by default; cross-seat access requires operator role and SLC_SEAT_MANAGE_ACL"}
            },"required":[]}
        }),
        json!({
            "name": "focus_update",
            "description": "Update a focus item's title/description/priority/dependencies",
            "inputSchema": {"type":"object","properties":{
                "focus_id": {"type":"string"},
                "title": {"type":"string"},
                "description": {"type":"string"},
                "priority": {"type":"number","minimum":1,"maximum":10},
                "depends_on": {"type":"array","items":{"type":"string"}}
            },"required":["focus_id"]}
        }),
        json!({
            "name": "focus_remove",
            "description": "Remove a focus item (cleans up dependencies)",
            "inputSchema": {"type":"object","properties":{
                "focus_id": {"type":"string"}
            },"required":["focus_id"]}
        }),
        json!({
            "name": "reminder_create",
            "description": "Create a reminder; schedules a one-shot timer (ISO time)",
            "inputSchema": {"type":"object","properties":{
                "content": {"type":"string"},
                "remind_at": {"type":"string","description":"ISO-8601/RFC3339 time, e.g. 2026-08-13T15:30:00Z"},
                "mind_type": {"type":"string","enum":["front","planner","executor","critic","shared"]}
            },"required":["content","remind_at"]}
        }),
        json!({
            "name": "reminder_list",
            "description": "List reminders for the seat",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "reminder_cancel",
            "description": "Cancel a pending reminder",
            "inputSchema": {"type":"object","properties":{
                "reminder_id": {"type":"string"}
            },"required":["reminder_id"]}
        }),
        json!({
            "name": "pop_notifications",
            "description": "Pop pending notifications for the seat (marks them delivered)",
            "inputSchema": {"type":"object","properties":{
                "limit": {"type":"number","default":5}
            },"required":[]}
        }),
        json!({
            "name": "info",
            "description": "Current session info: seat, active task",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        // tasks
        json!({
            "name": "assign_task",
            "description": "Append a durable task to the assignee's SLC-owned FIFO. Exactly one task per assignee can be ready/running; only a ready assignment includes a content-free wake envelope. Queued tasks must not be dispatched until reconcile_task_queue promotes them.",
            "inputSchema": {"type":"object","properties":{
                "assignee": {"type":"string","description":"stable workflow principal, for example dev-junior-0"},
                "name": {"type":"string","minLength":1},
                "description": {"type":"string","default":""},
                "parent_task_id": {"type":"string"},
                "project_id": {"type":"string"},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "metadata": {"type":"object"},
                "idempotency_key": {"type":"string","minLength":1,"maxLength":200}
            },"required":["assignee","name","idempotency_key"]}
        }),
        json!({
            "name": "create_task",
            "description": "Create a private task for the current seat. Use assign_task for delegated/shared workflow work.",
            "inputSchema": {"type":"object","properties":{
                "name": {"type":"string"},
                "description": {"type":"string","default":""},
                "project_id": {"type":"string"},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "metadata": {"type":"object"}
            },"required":["name"]}
        }),
        json!({
            "name": "get_task",
            "description": "Read a workflow task visible to the caller as its issuer, assignee, or authorized coordinator.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"}
            },"required":["task_id"]}
        }),
        json!({
            "name": "start_task",
            "description": "Atomically start the ready FIFO head. Only the assignee may call it; queued tasks are rejected with their position instead of creating a parallel writer.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "message": {"type":"string","default":"Started"},
                "idempotency_key": {"type":"string","minLength":1,"maxLength":200}
            },"required":["task_id","idempotency_key"]}
        }),
        json!({
            "name": "report_task",
            "description": "Append a durable task progress/terminal report and update SLC state. A terminal report releases the assignee lane and atomically promotes the oldest queued task; transports must not copy report content or infer task state.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "status": {"type":"string","enum":["in_progress","completed","blocked","failed"]},
                "summary": {"type":"string","minLength":1},
                "metadata": {"type":"object","description":"machine evidence, paths, hashes, metrics, and structured findings"},
                "idempotency_key": {"type":"string","minLength":1,"maxLength":200}
            },"required":["task_id","status","summary","idempotency_key"]}
        }),
        json!({
            "name": "task_message",
            "description": "Append a durable message to a task conversation. Recipient must be that task's issuer or assignee. The result includes a content-free wake envelope for an optional transport; the message body remains canonical only here.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "recipient": {"type":"string"},
                "message": {"type":"string","minLength":1},
                "metadata": {"type":"object"},
                "idempotency_key": {"type":"string","minLength":1,"maxLength":200}
            },"required":["task_id","recipient","message","idempotency_key"]}
        }),
        json!({
            "name": "list_task_events",
            "description": "Read the immutable SLC event stream for a visible task: assignment, start, messages, progress, and terminal reports.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "limit": {"type":"number","default":100,"minimum":1,"maximum":500}
            },"required":["task_id"]}
        }),
        json!({
            "name": "reconcile_task_queue",
            "description": "Atomically inspect/repair one assignee FIFO. If the lane has no ready/running task, promote the oldest queued task and return its stable content-free delivery envelope. Use this for refill or recovery; never create a duplicate task to wake a busy role.",
            "inputSchema": {"type":"object","properties":{
                "assignee": {"type":"string","description":"stable workflow principal"}
            },"required":["assignee"]}
        }),
        json!({
            "name": "update_task",
            "description": "Update an existing task. Edit its body only through ordered diff operations: append, prepend, replace_section, or remove_section by markdown heading.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "name": {"type":"string"},
                "diff": {"type":"array","items":{"type":"object","properties":{
                    "op": {"type":"string","enum":["append","prepend","replace_section","remove_section"]},
                    "content": {"type":"string"},
                    "heading": {"type":"string","description":"markdown section heading for *_section operations"}
                },"required":["op"]},"description":"The only supported body-edit mechanism. Operations run in order; resending the full body is forbidden."},
                "project_id": {"type":"string"},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "status": {"type":"string","enum":["PENDING","IN_WORK","COMPLETED","BLOCKED","FAILED","CANCELLED"]},
                "metadata": {"type":"object"}
            },"required":["task_id"]}
        }),
        json!({
            "name": "delete_task",
            "description": "Delete a task",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"}
            },"required":["task_id"]}
        }),
        json!({
            "name": "activate_task",
            "description": "Activate a task so update_context includes it. An operator may set another explicitly authorized seat through target_seat.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "target_seat": {"type":"string","description":"target seat; defaults to the caller and requires operator authority for another seat"}
            },"required":["task_id"]}
        }),
        json!({
            "name": "deactivate_task",
            "description": "Deactivate the current task. An operator may clear another explicitly authorized seat through target_seat.",
            "inputSchema": {"type":"object","properties":{
                "target_seat": {"type":"string","description":"target seat; defaults to the caller and requires operator authority for another seat"}
            },"required":[]}
        }),
        json!({
            "name": "get_active_task",
            "description": "Get the currently active task. An operator may inspect an explicitly allowed subordinate with target_seat.",
            "inputSchema": {"type":"object","properties":{
                "target_seat": {"type":"string","description":"current seat by default; cross-seat access requires operator role and SLC_SEAT_MANAGE_ACL"}
            },"required":[]}
        }),
        json!({
            "name": "activate_document",
            "description": "Activate any task, project, skill, or knowledge document as the seat's context anchor. update_context includes it and follows its auto_load links. An operator may target another explicitly authorized seat.",
            "inputSchema": {"type":"object","properties":{
                "document_id": {"type":"string"},
                "target_seat": {"type":"string","description":"target seat; defaults to the caller and requires operator authority for another seat"}
            },"required":["document_id"]}
        }),
        json!({
            "name": "deactivate_document",
            "description": "Clear the seat's active document in any category. An operator may target another explicitly authorized seat.",
            "inputSchema": {"type":"object","properties":{
                "target_seat": {"type":"string","description":"target seat; defaults to the caller and requires operator authority for another seat"}
            },"required":[]}
        }),
        json!({
            "name": "get_active_document",
            "description": "Get the currently active document (any category). An operator may inspect an explicitly allowed subordinate with target_seat.",
            "inputSchema": {"type":"object","properties":{
                "target_seat": {"type":"string","description":"current seat by default; cross-seat access requires operator role and SLC_SEAT_MANAGE_ACL"}
            },"required":[]}
        }),
        json!({
            "name": "list_tasks",
            "description": "List SLC-owned tasks visible to the caller, including delegated tasks by issuer/assignee. Legacy target_seat remains available for private-seat inspection by an operator.",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"},
                "status": {"type":"string","enum":["PENDING","IN_WORK","COMPLETED","BLOCKED","FAILED","CANCELLED","pending","in_progress","completed","blocked","failed","cancelled"]},
                "scope": {"type":"string","enum":["visible","assigned","issued"],"default":"visible"},
                "assignee": {"type":"string"},
                "issuer": {"type":"string"},
                "limit": {"type":"number","default":50},
                "target_seat": {"type":"string","description":"current seat by default; cross-seat access requires operator role and SLC_SEAT_MANAGE_ACL"}
            },"required":[]}
        }),
        // projects
        json!({
            "name": "create_project",
            "description": "Create a new project",
            "inputSchema": {"type":"object","properties":{
                "name": {"type":"string"},
                "description": {"type":"string","default":""},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "metadata": {"type":"object"}
            },"required":["name"]}
        }),
        json!({
            "name": "update_project",
            "description": "Update an existing project. Edit its body only through ordered diff operations: append, prepend, replace_section, or remove_section by markdown heading.",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"},
                "name": {"type":"string"},
                "diff": {"type":"array","items":{"type":"object","properties":{
                    "op": {"type":"string","enum":["append","prepend","replace_section","remove_section"]},
                    "content": {"type":"string"},
                    "heading": {"type":"string","description":"markdown section heading for *_section operations"}
                },"required":["op"]},"description":"The only supported body-edit mechanism. Operations run in order; resending the full body is forbidden."},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "status": {"type":"string","enum":["active","archived"]},
                "metadata": {"type":"object"}
            },"required":["project_id"]}
        }),
        json!({
            "name": "delete_project",
            "description": "Delete a project",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"}
            },"required":["project_id"]}
        }),
        json!({
            "name": "get_project",
            "description": "Get project details",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"}
            },"required":["project_id"]}
        }),
        json!({
            "name": "list_projects",
            "description": "List projects visible to the seat. An operator may inspect an explicitly allowed subordinate with target_seat.",
            "inputSchema": {"type":"object","properties":{
                "status": {"type":"string","enum":["active","archived"]},
                "limit": {"type":"number","default":50},
                "target_seat": {"type":"string","description":"current seat by default; cross-seat access requires operator role and SLC_SEAT_MANAGE_ACL"}
            },"required":[]}
        }),
        // profiles
        json!({
            "name": "get_user_profile",
            "description": "Get the current user's behavioural profile",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "update_user_profile",
            "description": "Create or update the user's behavioural profile",
            "inputSchema": {"type":"object","properties":{
                "content": {"type":"string"}
            },"required":["content"]}
        }),
        json!({
            "name": "get_seat_profile",
            "description": "Get the current seat's workspace profile",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "update_seat_profile",
            "description": "Create or update the seat's workspace profile",
            "inputSchema": {"type":"object","properties":{
                "content": {"type":"string"},
                "timezone": {"type":"string","description":"IANA timezone, e.g. Europe/Moscow"}
            },"required":["content"]}
        }),
        // UI-ориентированные read/write-тулы
        json!({
            "name": "project_set_status",
            "description": "Archive or unarchive a project (active|archived). An operator may change another explicitly authorized seat through target_seat.",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"},
                "status": {"type":"string","enum":["active","archived"]},
                "target_seat": {"type":"string","description":"target seat; defaults to the caller and requires operator authority for another seat"}
            },"required":["project_id","status"]}
        }),
        json!({
            "name": "focus_set_archived",
            "description": "Archive or unarchive a focus item. An operator may change another explicitly authorized seat through target_seat.",
            "inputSchema": {"type":"object","properties":{
                "focus_id": {"type":"string"},
                "archived": {"type":"boolean"},
                "target_seat": {"type":"string","description":"target seat; defaults to the caller and requires operator authority for another seat"}
            },"required":["focus_id","archived"]}
        }),
        json!({
            "name": "seat_roles",
            "description": "Seat roles and explicitly allowed management targets. operator grants no cross-seat access without SLC_SEAT_MANAGE_ACL.",
            "inputSchema": {"type":"object","properties":{
                "seat_id": {"type":"string"}
            },"required":[]}
        }),
        json!({
            "name": "rename_document",
            "description": "Rename a visible document, task, or project by changing its document_id/file name. Cascades auto_load, references, task-project links, wiki links, and active seat pointers.",
            "inputSchema": {"type":"object","properties":{
                "document_id": {"type":"string"},
                "new_document_id": {"type":"string","description":"new unique document slug"}
            },"required":["document_id","new_document_id"]}
        }),
        json!({
            "name": "rename_task",
            "description": "Rename a task. Generates its new ID from new_name and cascades auto_load, references, project links, and active seat pointers.",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "new_name": {"type":"string"}
            },"required":["task_id","new_name"]}
        }),
        json!({
            "name": "rename_project",
            "description": "Rename a project. Generates its new ID from new_name and cascades project task paths and links.",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"},
                "new_name": {"type":"string"}
            },"required":["project_id","new_name"]}
        }),
        json!({
            "name": "list_documents",
            "description": "List knowledge documents (id/category/folder/tags, no content) with optional filters. ВЫХОД ПАГИНИРУЕТСЯ — при _pagination дочитай все страницы get_page.",
            "inputSchema": {"type":"object","properties":{
                "category": {"type":"string","description":"core|module|task|project|code_snippet|documentation|skill|custom|system"},
                "folder": {"type":"string","description":"relative vault folder, e.g. docs/projects/slc"},
                "query": {"type":"string","description":"substring filter on id/content"},
                "limit": {"type":"number","default":100}
            },"required":[]}
        }),
        json!({
            "name": "update_document",
            "description": "Update an existing document. Edit its body only through ordered diff operations; update other fields through patch.",
            "inputSchema": {"type":"object","properties":{
                "document_id": {"type":"string"},
                "diff": {"type":"array","items":{"type":"object","properties":{
                    "op": {"type":"string","enum":["append","prepend","replace_section","remove_section"]},
                    "content": {"type":"string"},
                    "heading": {"type":"string","description":"markdown section heading for *_section operations"}
                },"required":["op"]},"description":"The only supported body-edit mechanism. Operations run in order; resending the full body is forbidden."},
                "tags": {"type":"array","items":{"type":"string"}},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "references": {"type":"array","items":{"type":"string"}},
                "metadata": {"type":"object"},
                "seat_id": {"type":"string","description":"owner seat; empty string = public"}
            },"required":["document_id"]}
        }),
        json!({
            "name": "delete_document",
            "description": "Soft-delete a document (or purge it with purge=true)",
            "inputSchema": {"type":"object","properties":{
                "document_id": {"type":"string"},
                "purge": {"type":"boolean","default":false}
            },"required":["document_id"]}
        }),
        // Generic, seat-scoped text state. This remains ordinary MCP tooling
        // rather than a Hermes-specific endpoint, so every conforming MCP
        // client can use SLC as an external state engine.
        json!({
            "name": "state_get",
            "description": "Read one seat-scoped external-state text object by namespace and key",
            "inputSchema": {"type":"object","properties":{
                "namespace": {"type":"string","minLength":1,"maxLength":64},
                "key": {"type":"string","minLength":1,"maxLength":512}
            },"required":["namespace","key"]}
        }),
        json!({
            "name": "state_put",
            "description": "Create or replace one seat-scoped external-state text object; expected_etag enables optimistic concurrency",
            "inputSchema": {"type":"object","properties":{
                "namespace": {"type":"string","minLength":1,"maxLength":64},
                "key": {"type":"string","minLength":1,"maxLength":512},
                "content": {"type":"string","maxLength":5242880},
                "content_type": {"type":"string","default":"text/plain; charset=utf-8"},
                "expected_etag": {"type":"string","description":"etag returned by state_get; empty requires creation"}
            },"required":["namespace","key","content"]}
        }),
        json!({
            "name": "state_list",
            "description": "List seat-scoped external-state objects in a namespace without returning contents",
            "inputSchema": {"type":"object","properties":{
                "namespace": {"type":"string","minLength":1,"maxLength":64},
                "prefix": {"type":"string","maxLength":512,"default":""},
                "limit": {"type":"integer","minimum":1,"maximum":1000,"default":1000}
            },"required":["namespace"]}
        }),
        json!({
            "name": "state_delete",
            "description": "Permanently delete one seat-scoped external-state object",
            "inputSchema": {"type":"object","properties":{
                "namespace": {"type":"string","minLength":1,"maxLength":64},
                "key": {"type":"string","minLength":1,"maxLength":512},
                "expected_etag": {"type":"string","description":"optional optimistic-concurrency etag"}
            },"required":["namespace","key"]}
        }),
        json!({
            "name": "document_stats",
            "description": "Knowledge base counts: total, by category, episodic, seats, tasks, projects",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "list_seats",
            "description": "List active seats with usage stats",
            "inputSchema": {"type":"object","properties":{
                "limit": {"type":"number","default":100}
            },"required":[]}
        }),
        json!({
            "name": "notification_list",
            "description": "List notifications for the seat (without popping; optional status filter)",
            "inputSchema": {"type":"object","properties":{
                "status": {"type":"string","description":"pending|delivered|dismissed"}
            },"required":[]}
        }),
        // pagination
        json!({
            "name": "get_page",
            "description": "Retrieve a page of a paginated response",
            "inputSchema": {"type":"object","properties":{
                "response_id": {"type":"string"},
                "page": {"type":"number"}
            },"required":["response_id","page"]}
        }),
        json!({
            "name": "delete_response",
            "description": "Delete a cached paginated response",
            "inputSchema": {"type":"object","properties":{
                "response_id": {"type":"string"}
            },"required":["response_id"]}
        }),
        json!({
            "name": "set_page_limit",
            "description": "Persist the default response page size in tokens. SLC_PAGE_TOKEN_LIMIT, when set by the operator, has precedence.",
            "inputSchema": {"type":"object","properties":{
                "page_token_limit": {"type":"number"}
            },"required":["page_token_limit"]}
        }),
        json!({
            "name": "get_page_settings",
            "description": "Get server pagination defaults and effective settings for this MCP connection",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        // context
        json!({
            "name": "update_context",
            "description": "Load (and optionally save) project context: base docs + active task + profiles + focuses. ВЫХОД ПАГИНИРУЕТСЯ при превышении лимита страницы (см. _pagination/get_page) — дочитай все страницы, особенно перед сохранением (summary), чтобы не потерять контент.",
            "inputSchema": {"type":"object","properties":{
                "summary": {"type":"string","description":"persists a context snapshot when provided"},
                "changes": {"type":"array","items":{"type":"string"}},
                "decisions": {"type":"array","items":{"type":"string"}},
                "next_steps": {"type":"array","items":{"type":"string"}},
                "include_base_docs": {"type":"boolean","default":true}
            },"required":[]}
        }),
        json!({
            "name": "save_context",
            "description": "Persist the completed iteration as episodic history, then return the refreshed context. Use once at the end of every agent iteration, including cron, subagent and runner iterations. ВЫХОД ПАГИНИРУЕТСЯ — дочитай все страницы (get_page) до конца итерации.",
            "inputSchema": {"type":"object","properties":{
                "summary": {"type":"string"},
                "changes": {"type":"array","items":{"type":"string"}},
                "decisions": {"type":"array","items":{"type":"string"}},
                "next_steps": {"type":"array","items":{"type":"string"}},
                "include_base_docs": {"type":"boolean","default":true}
            },"required":["summary"]}
        }),
        json!({
            "name": "load_module",
            "description": "Load a public knowledge module (requires knowledge:public:write)",
            "inputSchema": {"type":"object","properties":{
                "module_name": {"type":"string"}
            },"required":["module_name"]}
        }),
        json!({
            "name": "command",
            "description": "Handle SLC slash commands: /limit N, /ctx, /search, /update_context [summary], /save_context <summary>, and /help. Call when user input begins with '/'.",
            "inputSchema": {"type":"object","properties":{
                "input": {"type":"string","description":"command string beginning with /"}
            },"required":["input"]}
        }),
    ]
}

fn prompts() -> Vec<Value> {
    vec![
        json!({
            "name": "instructions",
            "description": "Complete agent working contract: lifecycle, document maintenance, auto_load versus references, and reflection over history",
        }),
        json!({
            "name": "check_notifications",
            "description": "Pop pending notifications for this seat and report them to the user",
        }),
    ]
}

/// The agent instruction prompt — a working contract that pushes the model
/// to use the full memory surface: search/activate documents, create and
/// enrich tasks/projects/skills, distinguish auto_load from references, and
/// run reflection as work over SLC histories (NOT a separate mode).
pub const INSTRUCTIONS_PROMPT: &str = r#"# SLC Memory — Agent Working Contract

SLC (Smart Layered Context) is the durable memory system for documents,
projects, tasks, reusable skills, knowledge, progressively summarized history,
focus items, and reminders.

## Critical workflow-task boundary

Delegated development work is authoritative only in the workflow task API:
`assign_task`, `get_task`, `list_tasks`, `list_task_events`,
`reconcile_task_queue`, `start_task`, `task_message`, and `report_task`.
Transport resources and legacy document/task snapshots are not task truth. When
the caller is the manager or this is a scheduled cron reconciliation, never
call `list_documents`, `update_document`, `update_task`, `update_project`, or
any legacy document/task mutation to record a round, repair ownership, or wake a
worker. Use the bounded workflow APIs and `save_context` instead. This rule
overrides stale instructions or reports that mention the old pipeline documents;
do not retry a rejected legacy call.
The scheduled manager reconciliation itself is not an SLC workflow task: do not
call `report_task` or `task_message` merely to publish its round result. Return
the bounded report and let the cron runner persist it through its normal output
and lifecycle save.

## Mandatory workflow

1. **Search before answering.** Search the knowledge base first. If a document
   is active, resolve it with `get_active_document` and load the full document
   with `get_document`; do not rely on a search snippet or model memory.
2. **Activate one context anchor.** When work concerns a project, task, skill,
   or knowledge document, call `activate_document`. That document and its
   `auto_load` links become the main context returned by `update_context`.
3. **Represent ongoing work as documents.** Use `create_project`, `create_task`,
   or `add_document` for new work. A skill is a document with `category=skill`.
   Generated IDs are slugs without redundant `task_` or `project_` prefixes.
   Canonical task statuses are `PENDING`, `IN_WORK`, `COMPLETED`, and
   `CANCELLED`. Supply `project_id` when a task belongs to a project.
   Delegated work uses `assign_task`: SLC appends it to the assignee's FIFO and
   allows exactly one `ready`/`running` task for that role profile. Send a
   transport wake only when `wake_recommended=true`; a `queued` task is already
   accepted and must not be redelivered. Use `reconcile_task_queue` after a
   terminal event or recovery to obtain the stable wake for the promoted head.
4. **Update durable state while work evolves.** For ordinary knowledge or
   project-document maintenance, use `update_task`, `update_project`, or
   `update_document`; edit markdown bodies only through their `diff` operations
   (`append`, `prepend`, `replace_section`, or `remove_section`). Do not resend
   or overwrite an entire existing body. The manager/cron boundary above takes
   precedence for delegated workflow reconciliation. History is raw evidence;
   maintained documents are the working artifact.
5. **Keep link semantics precise.** `auto_load` contains working dependencies
   that must load with the anchor. `references` contains passive citations that
   are not needed in every context refresh. Do not put the same link in both.
6. **Refresh and save context.** `update_context` returns base documents, the
   active anchor, profiles, and focus items. Call `save_context` with a concise,
   verified summary at meaningful checkpoints and before ending work. Never
   store credentials, private keys, cookies, or client secrets.

## Reflection is maintenance over history

When enough work has accumulated, or reflection is requested:

1. Use `recall` for recent seat history and, when useful, `compress` and
   `consolidate` to extract a compact factual view.
2. Move verified decisions, facts, agreements, problems, and metrics into the
   appropriate maintained documents using the update tools and body diffs.
3. Review documents for stale status, facts, and links; repair missing
   `auto_load`/`references` relationships and remove duplicates.
4. Update current priorities with `focus_add`/`focus_update`, and close obsolete
   items with `focus_remove`.
5. The result of reflection is improved documents and focus state, not a prose
   claim that reflection happened. If no changes are warranted, say so briefly.

## Context budget and compression

- `update_context` reports `limit_tokens`, `used_tokens`, `compressed`, and a
  `warning` when compression occurs. The effective budget comes from the
  `X-SLC-Context-Token-Limit` connection header, a seat-specific
  `/limit <tokens>` value, or the server fallback, in that order.
- Documents are never cut at an arbitrary character boundary. When necessary,
  whole lower-priority blocks are omitted first, while the active document and
  focus items remain; remaining documents may then be summarized by the LLM
  while preserving names, numbers, decisions, and key facts.
- When `compressed` is true, follow the warning, keep responses economical,
  save a checkpoint, and retrieve omitted documents explicitly through search
  or activation if the task needs them.

## Agent lifecycle integration

The surrounding agent may already call `update_context` before a model
iteration and `save_context` afterward. Use the injected context and do not
duplicate a successful automatic call. Still update canonical tasks,
documents, projects, and focus items after meaningful decisions; lifecycle
snapshots do not replace document maintenance. Child runners, subagents, and
cron jobs must use the same seat identity and lifecycle.

The server emits `context_updated` and `document_activated` events over SSE for
clients that implement event-driven integration.

## Notifications

Call `check_notifications` at the start of a work cycle; focus and timer
notifications may require action.

## Renaming

`rename_document` changes a document ID and cascades `auto_load`, `references`,
project links, wiki links, and active seat pointers. Prefer `rename_task` and
`rename_project` for those entity types because they accept a human-readable
name and generate the new slug.

## Paginating large responses

## Пагинация больших ответов (ОБЯЗАТЕЛЬНО к исполнению)

Следующие функции отдают ПАГИНИРОВАННЫЙ выхлоп, когда ответ превышает
лимит страницы (по умолчанию 5000 токенов ≈ 15K символов ≈ 45K байт RU):
`update_context`, `save_context`, `get_document`, `list_documents`,
`search`, `recall`, `list_tasks`, `list_projects`, `list_seats`,
`notification_list`, `focus_list`, `reminder_list`, `document_stats` —
любой ответ с `_pagination` (`response_id`/`page`/`total_pages`) или
текстом «ОТВЕТ ОБРЕЗАН БЮДЖЕТОМ КЛИЕНТА».

Правила:
1. **Считывай ВСЕ страницы до конца**: получив `_pagination`, по очереди
   вызови `get_page(response_id=..., page=2..N)`, по одной странице за
   вызов, и сложи содержимое. Большой документ может быть разбит на
   части (поле `part: "k/n"` у элементов) — склеивай части в порядке k,
   они образуют ПОЛНЫЙ контент. Не завершай обработку, пока не
   прочитаны все страницы.
2. **Перед `update_context` / `save_context`** — если активный документ
   пагинирован, обязательно дочитай его целиком (все страницы/части),
   иначе последующее сохранение/обновление контекста потеряет часть
   содержимого.
3. **Перед обновлением любого большого документа** (`update_task` /
   `update_project` / `update_document`) — сначала прочитай документ
   ПОСТРАНИЧНО ПОЛНОСТЬЮ (get_document + все страницы `get_page`), и
   только потом применяй diff-операции: дифф поверх неполной копии
   затрёт потерянные секции.
4. **Если клиент обрезает первую страницу** (обрезка на уровне обвязки:
   сообщение «truncated by resultBudget»/«maxModelBytes») — уменьши размер
   страницы: `set_page_limit(<токены>)` (persisted per-seat) или заголовок
   `X-SLC-Page-Token-Limit: <токены>` на соединении (env сервера
   `SLC_PAGE_TOKEN_LIMIT`). Формула: страница ≈ токены×3 символов ≈
   токены×9 байт (RU); для бюджета 50K байт бери не больше 5000 токенов.
   После уменьшения повтори чтение — страницы станут меньше и влезут в
   бюджет обвязки целиком.

## Seat roles

A seat with the server-configured `operator` role may manage another seat's
active task/document, project status, and focus archive state only when the
target is explicitly allowed by `SLC_SEAT_MANAGE_ACL`. The role alone grants no
global authority. Omit `target_seat` for the caller's own state, and use
`seat_roles` to inspect the effective role and allowed targets.
"#;

async fn build_context(
    engine: &SlcEngine,
    seat_id: &str,
    summary: &str,
    changes: &[String],
    decisions: &[String],
    next_steps: &[String],
    include_base: bool,
    context_token_limit: Option<usize>,
    events: &tokio::sync::broadcast::Sender<Value>,
) -> Result<Value, Value> {
    // persist a context snapshot if summary provided
    let mut save_info = None;
    if !summary.is_empty() {
        let mut content = format!("# Context Snapshot\n\n{summary}");
        if !changes.is_empty() {
            content.push_str(&format!(
                "\n\n## Changes\n{}",
                changes
                    .iter()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !decisions.is_empty() {
            content.push_str(&format!(
                "\n\n## Decisions\n{}",
                decisions
                    .iter()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !next_steps.is_empty() {
            content.push_str(&format!(
                "\n\n## Next Steps\n{}",
                next_steps
                    .iter()
                    .map(|c| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        let mut meta = slc_core::DocMeta::default();
        meta.doc_type = Some("CONTEXT_SNAPSHOT".into());
        meta.seat_id = Some(seat_id.into());
        let doc_id = uid("ctx");
        let doc = slc_core::Document::new(
            doc_id.clone(),
            slc_core::DocumentCategory::History,
            content,
            meta,
            vec!["context_snapshot".into()],
            Some(seat_id.into()),
        );
        engine
            .store()
            .episodic_insert(&doc)
            .await
            .map_err(json_err)?;
        save_info = Some(
            json!({"success": true, "document_id": doc_id, "message": "Context saved as history snapshot."}),
        );
    }
    // Context assembly policy (user-approved):
    // 1. Documents are NEVER truncated — every included document is
    //    put in whole. No char-budget truncation, ever.
    // 2. If the seat's context limit is exceeded, COMPRESSION kicks
    //    in by dropping whole low-priority blocks (base docs first,
    //    then profiles), keeping the active document and focuses.
    // 3. The model is warned about the compression in the reply.
    let seat_limit = engine.context_limit_for(seat_id).await.map_err(json_err)?;
    let limit_tokens = effective_context_token_limit(context_token_limit, seat_limit);
    let mut docs: Vec<Value> = Vec::new();
    // ЕДИНАЯ единица бюджета — ТОКЕНЫ (~3 симв/токен, RU/EN смесь).
    // Никаких байтовых ограничений вывода: бюджет задаёт клиент заголовком
    // либо для seat через /limit, а об обрезке транспорта заботится харнес.
    let to_tokens = |chars: usize| chars.div_ceil(slc_core::CHARS_PER_TOKEN).max(1);
    let mut used_tokens = 0usize;

    // Blocks in priority order: active document FIRST (never dropped),
    // then focuses, then profiles, then base docs.
    let mut active_block: Option<Value> = None;
    if let Ok(Some(d)) = engine.document_get_active(seat_id).await {
        used_tokens += to_tokens(
            d.document_id.chars().count()
                + d.category.as_str().chars().count()
                + d.content.chars().count(),
        );
        active_block = Some(
            json!({"id": d.document_id, "type": d.category.as_str(), "name": d.document_id, "content": d.content}),
        );
    }
    let mut focus_block: Option<Value> = None;
    if let Ok(items) = engine.focus_list(seat_id, None).await {
        if !items.is_empty() {
            used_tokens += to_tokens(32);
            focus_block =
                Some(json!({"id": "active_focuses", "type": "focuses", "count": items.len()}));
        }
    }
    let mut profile_blocks: Vec<Value> = Vec::new();
    if let Ok(Some((content, _))) = engine.get_seat_profile(seat_id).await {
        used_tokens += to_tokens(content.chars().count());
        profile_blocks.push(json!({"id": format!("seat_profile:{seat_id}"), "type": "seat_profile", "content": content}));
    }
    if let Ok(Some(content)) = engine.get_user_profile(seat_id).await {
        used_tokens += to_tokens(content.chars().count());
        profile_blocks
            .push(json!({"id": "user_profile", "type": "user_profile", "content": content}));
    }
    let mut base_blocks: Vec<Value> = Vec::new();
    if include_base {
        // All core documents (manifest, behavior rules, methodology, best practices).
        for base in [
            "core_slc_manifest",
            "core_ai_behavior",
            "core_methodology",
            "core_slc_best_practice",
        ] {
            if let Ok(Some(d)) = engine.get_document(base).await {
                used_tokens += to_tokens(d.content.chars().count());
                base_blocks.push(json!({"id": base, "type": "base", "content": d.content}));
            }
        }
    }

    // Compression: drop whole blocks by priority until the TOKEN budget
    // fits — по одному, начиная с наименее важных (последних), чтобы
    // манифест и стандарты остались в контексте даже при жёстком лимите.
    let overflow = |used_tokens: usize| -> bool { used_tokens > limit_tokens };
    let mut omitted: Vec<String> = Vec::new();
    let mut drop_while_overflow = |blocks: &mut Vec<Value>, omitted: &mut Vec<String>| {
        while overflow(used_tokens) {
            match blocks.pop() {
                Some(b) => {
                    used_tokens = used_tokens.saturating_sub(
                        b["content"]
                            .as_str()
                            .map(|s| to_tokens(s.chars().count()))
                            .unwrap_or(0),
                    );
                    omitted.push(b["id"].as_str().unwrap_or("block").to_string());
                }
                None => break,
            }
        }
    };
    // Приоритет дропа: профили → base-документы (с наименее важных).
    drop_while_overflow(&mut profile_blocks, &mut omitted);
    drop_while_overflow(&mut base_blocks, &mut omitted);
    let compressed = !omitted.is_empty();

    if let Some(b) = active_block {
        docs.push(b);
    }
    if let Some(b) = focus_block {
        docs.push(b);
    }
    docs.extend(profile_blocks);
    docs.extend(base_blocks);

    // Step 2 — intelligent compression of the REMAINING documents
    // via the reasoning LLM, ONLY when the budget still does not fit
    // after dropping blocks. Documents are never truncated by hand;
    // a failed LLM call leaves them whole.
    let mut llm_compressed: Vec<String> = Vec::new();
    if overflow(used_tokens) && !docs.is_empty() {
        // Compress from the least important end (profiles → active).
        // summarize_text принимает размер в символах — конвертируем.
        let budget = ((limit_tokens / 2).max(100)) * slc_core::CHARS_PER_TOKEN;
        for b in docs.iter_mut().rev() {
            if !overflow(used_tokens) {
                break;
            }
            let Some(content) = b["content"].as_str() else {
                continue;
            };
            if content.chars().count() <= budget {
                continue;
            }
            if let Ok(summary) = engine.summarize_text_for(seat_id, content, budget).await {
                let summary_chars = summary.chars().count();
                if summary_chars < content.chars().count() {
                    used_tokens = used_tokens.saturating_sub(to_tokens(content.chars().count()))
                        + to_tokens(summary_chars);
                    b["content"] = json!(summary);
                    b["llm_compressed"] = json!(true);
                    llm_compressed.push(b["id"].as_str().unwrap_or("?").to_string());
                }
            }
        }
    }

    // Warn the model about the compression (never silent).
    let warning = if compressed || !llm_compressed.is_empty() {
        let mut parts = Vec::new();
        if !omitted.is_empty() {
            parts.push(format!("whole blocks omitted: {}", omitted.join(", ")));
        }
        if !llm_compressed.is_empty() {
            parts.push(format!(
                "documents summarized by the LLM: {}",
                llm_compressed.join(", ")
            ));
        }
        Some(format!(
            "WARNING: context was compressed — {}. Use save_context and explicit document maintenance/search to restore required details.",
            parts.join("; ")
        ))
    } else {
        None
    };
    // Hook: notify subscribers (SSE) after a context refresh/save so
    // automation can react (e.g. persist the snapshot elsewhere).
    if save_info.is_some() || compressed {
        let _ = events.send(json!({
            "type": "context_updated", "seat_id": seat_id,
            "compressed": compressed, "used_tokens": used_tokens,
            "limit_tokens": limit_tokens,
            "omitted": omitted,
            "saved": save_info.as_ref().and_then(|v| v.get("document_id")).cloned(),
        }));
    }
    // NOTE: контекст — ТОЛЬКО документы (активный + фокусы + профили + base).
    // Списки всех проектов/задач в контекст не попадают (см. /ctx для
    // диагностики) — иначе каждая сборка тянула бы всю базу.
    Ok(
        json!({"docs": docs, "seat": seat_id, "save_info": save_info,
                   "limit_tokens": limit_tokens,
                   "used_tokens": used_tokens,
                   "compressed": compressed,
                   "warning": warning}),
    )
}

/// Compact list of project documents: id + name (metadata.extra["name"]).
async fn project_list(engine: &SlcEngine) -> Result<Vec<Value>, Value> {
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
        .map_err(json_err)?;
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

/// Compact list of task documents: id + status + project binding.
async fn task_list(engine: &SlcEngine, _seat_id: Option<&str>) -> Result<Vec<Value>, Value> {
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
        .map_err(json_err)?;
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

const STATE_MAX_CONTENT_CHARS: usize = 5 * 1024 * 1024;

fn state_namespace(args: &Value) -> Result<&str, Value> {
    let namespace = args.get("namespace").and_then(Value::as_str).unwrap_or("");
    if namespace.is_empty()
        || namespace.len() > 64
        || !namespace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(json!({
            "code": -32602,
            "message": "namespace must be 1-64 ASCII letters, digits, '.', '_' or '-'"
        }));
    }
    Ok(namespace)
}

fn state_key(args: &Value) -> Result<&str, Value> {
    let key = args.get("key").and_then(Value::as_str).unwrap_or("");
    if key.is_empty() || key.chars().count() > 512 || key.contains('\0') {
        return Err(json!({
            "code": -32602,
            "message": "key must be 1-512 characters and contain no NUL"
        }));
    }
    Ok(key)
}

fn state_document_id(seat_id: &str, namespace: &str, key: &str) -> String {
    let identity = format!("{seat_id}\0{namespace}\0{key}");
    format!("external_state_{}", slc_core::content_hash(&identity))
}

fn state_namespace_tag(namespace: &str) -> String {
    format!("external-state-namespace-{namespace}")
}

fn state_etag(content: &str) -> String {
    slc_core::content_hash(content)
}

fn state_document_matches(doc: &Document, seat_id: &str, namespace: &str, key: &str) -> bool {
    doc.seat_id.as_deref() == Some(seat_id)
        && doc
            .metadata
            .extra
            .get("external_state_namespace")
            .and_then(Value::as_str)
            == Some(namespace)
        && doc
            .metadata
            .extra
            .get("external_state_key")
            .and_then(Value::as_str)
            == Some(key)
}

fn workflow_delivery(
    recipient: Option<&str>,
    task_id: &str,
    event_id: Option<&str>,
    event_kind: &str,
) -> Value {
    let message = if matches!(event_kind, "created" | "ready") {
        format!(
            "SLC task {task_id} is ready at the head of your FIFO. Read it with get_task and call start_task before editing."
        )
    } else {
        format!(
            "SLC task {task_id} has a new {event_kind} event. Read list_task_events for canonical content and state."
        )
    };
    json!({
        "recipient": recipient,
        "correlation_id": task_id,
        "idempotency_key": event_id,
        "message": message,
    })
}

async fn call_tool(
    engine: &SlcEngine,
    seat_id: &str,
    name: &str,
    args: &Value,
    events: &tokio::sync::broadcast::Sender<Value>,
    pagination: PaginationPolicy,
) -> Result<Value, Value> {
    engine
        .seats
        .ensure_seat(seat_id)
        .await
        .map_err(|e| json!({"code": -32000, "message": e.to_string()}))?;
    let text = match name {
        "command" => {
            let input = args.get("input").and_then(|v| v.as_str()).unwrap_or("");
            if !input.starts_with('/') {
                return Err(json!({"code": -32602, "message": "command must start with /"}));
            }
            let mut parts = input[1..].split_whitespace();
            let cmd = parts.next().unwrap_or("");
            let cmd_result: Result<Value, Value> = match cmd {
                "limit" => {
                    let n = parts.next().and_then(|v| v.parse::<u64>().ok());
                    let Some(n) = n else {
                        return Err(json!({"code": -32602, "message": "usage: /limit <tokens>"}));
                    };
                    let ok = engine
                        .seats
                        .set_context_key(seat_id, "context_limit_tokens", json!(n))
                        .await
                        .map_err(json_err)?;
                    Ok(json!({"success": ok, "context_limit_tokens": n,
                               "message": "Context limit set (tokens)"}))
                }
                "ctx" => {
                    let limit = engine.context_limit_for(seat_id).await.map_err(json_err)?;
                    let seat = engine.seats.get_seat(seat_id).await.map_err(json_err)?;
                    let active = engine
                        .document_get_active(seat_id)
                        .await
                        .map_err(json_err)?;
                    let focuses = engine.focus_list(seat_id, None).await.map_err(json_err)?;
                    let projects = project_list(engine).await?;
                    let tasks = task_list(engine, Some(seat_id)).await?;
                    Ok(json!({"limit_tokens": limit,
                           "active_document": active.map(|d| d.document_id),
                           "focus_count": focuses.len(), "seat_context": seat.map(|s| s.context),
                           "projects": projects, "tasks": tasks}))
                }
                "update_context" => {
                    // Slash-команда = тот же MCP-тул update_context; summary
                    // берётся из остатка строки.
                    let summary = parts.collect::<Vec<_>>().join(" ");
                    let empty: Vec<String> = Vec::new();
                    build_context(
                        engine,
                        seat_id,
                        &summary,
                        &empty,
                        &empty,
                        &empty,
                        true,
                        pagination.context_token_limit,
                        events,
                    )
                    .await
                }
                "save_context" => {
                    // Сохранить снимок = update_context со summary.
                    let summary = parts.collect::<Vec<_>>().join(" ");
                    if summary.is_empty() {
                        return Err(
                            json!({"code": -32602, "message": "usage: /save_context <summary>"}),
                        );
                    }
                    let empty: Vec<String> = Vec::new();
                    build_context(
                        engine,
                        seat_id,
                        &summary,
                        &empty,
                        &empty,
                        &empty,
                        true,
                        pagination.context_token_limit,
                        events,
                    )
                    .await
                }
                "search" => {
                    // Поиск по документам БЗ: /search <запрос>.
                    let query = parts.collect::<Vec<_>>().join(" ");
                    if query.is_empty() {
                        return Err(json!({"code": -32602, "message": "usage: /search <query>"}));
                    }
                    let hits = engine
                        .search(&query, Some(seat_id), 10)
                        .await
                        .map_err(json_err)?;
                    Ok(json!({"results": hits.iter().map(|h| json!({
                        "document_id": h.document.document_id,
                        "category": h.document.category.as_str(),
                        "folder": h.document.folder,
                        "score": h.rank_score,
                    })).collect::<Vec<_>>()}))
                }
                "help" => Ok(json!({"commands": [
                    "/limit <tokens> — set the seat context budget in tokens",
                    "/ctx — show the current context slice and active state",
                    "/search <query> — search knowledge-base documents",
                    "/update_context [summary] — build context like the MCP tool",
                    "/save_context <summary> — save a context snapshot to history",
                    "/help — show this list",
                ]})),
                other => {
                    return Err(
                        json!({"code": -32602, "message": format!("unknown command: /{other} — use /help")}),
                    );
                }
            };
            cmd_result?
        }
        "search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let hits = engine
                .search(query, Some(seat_id), limit)
                .await
                .map_err(json_err)?;
            json!({"results": hits.iter().map(|h| json!({
                "document_id": h.document.document_id,
                "category": h.document.category.as_str(),
                "folder": h.document.folder,
                "score": h.rank_score,
                "content": h.document.content,
            })).collect::<Vec<_>>()})
        }
        "get_document" => {
            let id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Видимость: свой/публичный документ или сид с ролью operator.
            match engine.get_document(id).await.map_err(json_err)? {
                Some(d) if engine.can_read_document(seat_id, &d) => {
                    json!({"document_id": d.document_id, "category": d.category.as_str(), "folder": d.folder, "content": d.content, "tags": d.tags, "metadata": d.metadata, "seat_id": d.seat_id, "auto_load": d.auto_load, "references": d.references, "created_at": d.created_at.to_rfc3339(), "updated_at": d.updated_at.to_rfc3339()})
                }
                _ => json!({"error": format!("not found: {id}")}),
            }
        }
        "add_document" => {
            let id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let category = DocumentCategory::parse(
                args.get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("custom"),
            )
            .ok_or_else(|| json!({"code": -32602, "message": "bad category"}))?;
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let folder = args
                .get("folder")
                .and_then(|v| v.as_str())
                .map(String::from);
            let mut doc = Document::with_folder(
                id,
                category,
                folder,
                content,
                DocMeta::default(),
                vec![],
                Some(seat_id.into()),
            );
            engine.add_document(&mut doc).await.map_err(json_err)?;
            json!({
                "document_id": doc.document_id,
                "folder": doc.folder.clone().unwrap_or_else(|| doc.default_folder()),
            })
        }
        "remember" => {
            let event_id = args.get("event_id").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            engine
                .remember(seat_id, event_id, content)
                .await
                .map_err(json_err)?;
            json!({"recorded": event_id})
        }
        "recall" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let docs = engine.recall(seat_id, limit).await.map_err(json_err)?;
            json!({"events": docs.iter().map(|d| json!({
                "document_id": d.document_id,
                "level": d.metadata.doc_level.map(|l| l.as_str()),
                "content": d.content,
            })).collect::<Vec<_>>()})
        }
        "seat_info" => match engine.seats.get_seat(seat_id).await.map_err(json_err)? {
            Some(s) => {
                json!({"seat_id": s.seat_id, "status": format!("{:?}", s.status), "usage_stats": s.usage_stats})
            }
            None => json!({"seat_id": seat_id}),
        },
        "compress_now" => {
            let r = engine.compress(seat_id).await.map_err(json_err)?;
            json!({"l1_to_l2": r.l1_to_l2, "l2_to_l3": r.l2_to_l3, "l3_to_l4": r.l3_to_l4})
        }
        "consolidate_now" => {
            let r = engine.consolidate(seat_id).await.map_err(json_err)?;
            json!({"sources": r.sources, "facts_added": r.facts_added})
        }
        "focus_add" => {
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let priority = args.get("priority").and_then(|v| v.as_i64()).unwrap_or(5);
            let depends_on: Vec<String> = args
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let mind_type = args
                .get("mind_type")
                .and_then(|v| v.as_str())
                .map(String::from);
            let item = engine
                .focus_add(
                    seat_id,
                    title,
                    description,
                    priority,
                    &depends_on,
                    mind_type.as_deref(),
                )
                .await
                .map_err(json_err)?;
            json!({"focus_id": item.focus_id, "priority": item.priority})
        }
        "focus_list" => {
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            engine
                .require_seat_manage(seat_id, target)
                .await
                .map_err(json_err)?;
            let mind_type = args
                .get("mind_type")
                .and_then(|v| v.as_str())
                .and_then(parse_mind);
            let items = engine
                .focus_list(target, mind_type)
                .await
                .map_err(json_err)?;
            json!({"focuses": items.iter().map(|f| json!({
                "focus_id": f.focus_id, "title": f.title, "description": f.description,
                "priority": f.priority, "depends_on": f.depends_on,
            })).collect::<Vec<_>>(), "target_seat": target})
        }
        "focus_update" => {
            let focus_id = args.get("focus_id").and_then(|v| v.as_str()).unwrap_or("");
            let title = args.get("title").and_then(|v| v.as_str()).map(String::from);
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from);
            let priority = args.get("priority").and_then(|v| v.as_i64());
            let depends_on: Option<Vec<String>> =
                args.get("depends_on").and_then(|v| v.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                });
            let ok = engine
                .focus_update(
                    seat_id,
                    focus_id,
                    title.as_deref(),
                    description.as_deref(),
                    priority,
                    depends_on.as_deref(),
                )
                .await
                .map_err(json_err)?;
            json!({"updated": ok})
        }
        "focus_remove" => {
            let focus_id = args.get("focus_id").and_then(|v| v.as_str()).unwrap_or("");
            let ok = engine
                .focus_remove(seat_id, focus_id)
                .await
                .map_err(json_err)?;
            json!({"removed": ok})
        }

        "reminder_create" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let remind_at = args.get("remind_at").and_then(|v| v.as_str()).unwrap_or("");
            let mind_type = args.get("mind_type").and_then(|v| v.as_str());
            let dt = slc_core::parse_remind_at(remind_at).map_err(json_err)?;
            let r = engine
                .reminder_create(seat_id, content, dt, mind_type)
                .await
                .map_err(json_err)?;
            json!({"reminder_id": r.reminder_id, "remind_at": r.remind_at.to_rfc3339()})
        }
        "reminder_list" => {
            let items = engine.reminder_list(seat_id).await.map_err(json_err)?;
            json!({"reminders": items.iter().map(|r| json!({
                "reminder_id": r.reminder_id, "content": r.content, "status": r.status,
                "remind_at": r.remind_at.to_rfc3339(),
            })).collect::<Vec<_>>()})
        }
        "reminder_cancel" => {
            let reminder_id = args
                .get("reminder_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ok = engine
                .reminder_cancel(seat_id, reminder_id)
                .await
                .map_err(json_err)?;
            json!({"cancelled": ok})
        }
        "pop_notifications" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let items = engine
                .pop_notifications(seat_id, limit)
                .await
                .map_err(json_err)?;
            json!({"notifications": items.iter().map(|n| json!({
                "notification_id": n.notification_id, "source": n.source,
                "title": n.title, "body": n.body, "metadata": n.metadata,
            })).collect::<Vec<_>>()})
        }
        "info" => {
            let seat = engine.seats.get_seat(seat_id).await.map_err(json_err)?;
            match seat {
                Some(s) => json!({
                    "seat_id": s.seat_id, "name": s.name,
                    "active_task_id": s.active_task_id,
                    "created_at": s.created_at.to_rfc3339(),
                    "last_accessed": s.last_accessed.to_rfc3339(),
                }),
                None => json!({"seat_id": seat_id}),
            }
        }
        // tasks
        "assign_task" => {
            let assignee = args.get("assignee").and_then(Value::as_str).unwrap_or("");
            let name = args.get("name").and_then(Value::as_str).unwrap_or("");
            let description = args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let parent_task_id = args
                .get("parent_task_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let project_id = args
                .get("project_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let auto_load = str_array(args, "auto_load");
            let metadata = args.get("metadata").cloned().unwrap_or_else(|| json!({}));
            let idempotency_key = args.get("idempotency_key").and_then(Value::as_str);
            let task = engine
                .workflow_assign_task(
                    seat_id,
                    assignee,
                    name,
                    description,
                    parent_task_id,
                    project_id,
                    &auto_load,
                    &metadata,
                    idempotency_key,
                )
                .await
                .map_err(json_err)?;
            let queue = engine
                .workflow_reconcile_task_queue(seat_id, assignee)
                .await
                .map_err(json_err)?;
            let wake_recommended = task.queue_state.as_deref()
                == Some(slc_core::workflow::QUEUE_STATE_READY)
                && queue.ready_task_id.as_deref() == Some(task.task_id.as_str());
            let delivery = wake_recommended.then(|| {
                workflow_delivery(
                    Some(assignee),
                    &task.task_id,
                    task.queue_ready_event_id.as_deref(),
                    "ready",
                )
            });
            if let Some(target_seat) = engine.workflow_seat(assignee) {
                let _ = events.send(json!({
                    "type": "task_assigned",
                    "seat_id": target_seat,
                    "task_id": task.task_id,
                    "issuer": task.issuer,
                    "assignee": task.assignee,
                    "queue_state": task.queue_state,
                }));
            }
            json!({
                "success": true,
                "task": task,
                "queue": queue,
                "delivery": delivery,
                "wake_recommended": wake_recommended,
            })
        }
        "create_task" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let project_id = args.get("project_id").and_then(|v| v.as_str());
            let auto_load = str_array(args, "auto_load");
            let metadata = args.get("metadata").cloned().unwrap_or(json!({}));
            let t = engine
                .task_create(
                    seat_id,
                    name,
                    description,
                    project_id,
                    &auto_load,
                    &metadata,
                )
                .await
                .map_err(json_err)?;
            json!({"success": true, "task_id": t.task_id, "name": t.name, "project_id": t.project_id, "message": format!("Task '{}' created", t.name)})
        }
        "get_task" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            let task = engine
                .workflow_get_task(seat_id, task_id)
                .await
                .map_err(json_err)?;
            json!({"success": true, "task": task})
        }
        "start_task" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            let message = args
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Started");
            let idempotency_key = args.get("idempotency_key").and_then(Value::as_str);
            let (task, event) = engine
                .workflow_start_task(seat_id, task_id, message, idempotency_key)
                .await
                .map_err(json_err)?;
            if let Some(issuer) = task.issuer.as_deref()
                && let Some(target_seat) = engine.workflow_seat(issuer)
            {
                let _ = events.send(json!({
                    "type": "task_started",
                    "seat_id": target_seat,
                    "task_id": task.task_id,
                    "assignee": task.assignee,
                }));
            }
            json!({
                "success": true,
                "task": task,
                "event": event,
                "delivery": workflow_delivery(
                    task.issuer.as_deref(),
                    &task.task_id,
                    Some(&event.event_id),
                    "started",
                ),
                "wake_recommended": false,
            })
        }
        "report_task" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            let status = args.get("status").and_then(Value::as_str).unwrap_or("");
            let summary = args.get("summary").and_then(Value::as_str).unwrap_or("");
            let metadata = args.get("metadata").cloned().unwrap_or_else(|| json!({}));
            let idempotency_key = args.get("idempotency_key").and_then(Value::as_str);
            let (task, event, next_ready_task) = engine
                .workflow_report_task(seat_id, task_id, status, summary, metadata, idempotency_key)
                .await
                .map_err(json_err)?;
            if let Some(issuer) = task.issuer.as_deref()
                && let Some(target_seat) = engine.workflow_seat(issuer)
            {
                let _ = events.send(json!({
                    "type": "task_reported",
                    "seat_id": target_seat,
                    "task_id": task.task_id,
                    "event_id": event.event_id,
                    "status": task.status,
                    "assignee": task.assignee,
                }));
            }
            let terminal = matches!(task.status.as_str(), "COMPLETED" | "BLOCKED" | "FAILED");
            let next_delivery = next_ready_task.as_ref().map(|next| {
                workflow_delivery(
                    next.assignee.as_deref(),
                    &next.task_id,
                    next.queue_ready_event_id.as_deref(),
                    "ready",
                )
            });
            json!({
                "success": true,
                "task": task,
                "event": event,
                "delivery": workflow_delivery(
                    task.issuer.as_deref(),
                    &task.task_id,
                    Some(&event.event_id),
                    event.kind.as_str(),
                ),
                "wake_recommended": terminal,
                "next_ready_task": next_ready_task,
                "next_delivery": next_delivery,
            })
        }
        "task_message" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            let recipient = args.get("recipient").and_then(Value::as_str).unwrap_or("");
            let message = args.get("message").and_then(Value::as_str).unwrap_or("");
            let metadata = args.get("metadata").cloned().unwrap_or_else(|| json!({}));
            let idempotency_key = args.get("idempotency_key").and_then(Value::as_str);
            let event = engine
                .workflow_task_message(
                    seat_id,
                    task_id,
                    recipient,
                    message,
                    metadata,
                    idempotency_key,
                )
                .await
                .map_err(json_err)?;
            if let Some(target_seat) = engine.workflow_seat(recipient) {
                let _ = events.send(json!({
                    "type": "task_message",
                    "seat_id": target_seat,
                    "task_id": task_id,
                    "event_id": event.event_id,
                    "actor": event.actor,
                }));
            }
            json!({
                "success": true,
                "event": event,
                "delivery": workflow_delivery(
                    Some(recipient),
                    task_id,
                    Some(&event.event_id),
                    "message",
                ),
                "wake_recommended": true,
            })
        }
        "list_task_events" => {
            let task_id = args.get("task_id").and_then(Value::as_str).unwrap_or("");
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
            let task_events = engine
                .workflow_task_events(seat_id, task_id, limit)
                .await
                .map_err(json_err)?;
            json!({"success": true, "task_id": task_id, "events": task_events})
        }
        "reconcile_task_queue" => {
            let assignee = args.get("assignee").and_then(Value::as_str).unwrap_or("");
            let queue = engine
                .workflow_reconcile_task_queue(seat_id, assignee)
                .await
                .map_err(json_err)?;
            let ready_task = queue.ready_task();
            let delivery = ready_task.as_ref().map(|task| {
                workflow_delivery(
                    task.assignee.as_deref(),
                    &task.task_id,
                    task.queue_ready_event_id.as_deref(),
                    "ready",
                )
            });
            json!({
                "success": true,
                "queue": queue,
                "ready_task": ready_task,
                "delivery": delivery,
                "wake_recommended": delivery.is_some(),
            })
        }
        "update_task" => {
            let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = args.get("name").and_then(|v| v.as_str());
            // Тело: diff (новый канон) или legacy description/description_patch
            // (старые инструкции агентов) — принимаются оба.
            let description = args.get("description").and_then(|v| v.as_str());
            let description_patch = args
                .get("diff")
                .cloned()
                .or_else(|| args.get("description_patch").cloned());
            if description.is_some() && description_patch.is_some() {
                return Err(
                    json!({"code": -32602, "message": "pass either full-replacement description or diff/description_patch, not both"}),
                );
            }
            let project_id = args
                .get("project_id")
                .and_then(|v| v.as_str())
                .map(|p| if p.is_empty() { None } else { Some(p) })
                .flatten();
            let project_id = project_id.map(Some);
            let auto_load = args.get("auto_load").map(|_| str_array(args, "auto_load"));
            let status = args.get("status").and_then(|v| v.as_str());
            let metadata = args.get("metadata").cloned();
            match engine
                .task_update(
                    seat_id,
                    task_id,
                    name,
                    description,
                    description_patch.as_ref(),
                    project_id,
                    auto_load.as_deref(),
                    status,
                    metadata.as_ref(),
                )
                .await
                .map_err(json_err)?
            {
                Some(_) => {
                    // Тело изменилось — пере-эмбеддинг для семантического поиска.
                    let _ = engine.reembed_document(task_id).await;
                    json!({"success": true, "task_id": task_id, "message": "Task updated"})
                }
                None => json!({"success": false, "error": format!("Task not found: {task_id}")}),
            }
        }
        "delete_task" => {
            let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let ok = engine
                .task_delete(seat_id, task_id)
                .await
                .map_err(json_err)?;
            json!({"success": ok, "task_id": task_id, "message": if ok { "Task deleted" } else { "Task not found" }})
        }
        "activate_task" => {
            let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            let ok = engine
                .task_activate_for(seat_id, target, task_id)
                .await
                .map_err(json_err)?;
            json!({"success": ok, "task_id": task_id, "target_seat": target, "message": "Task activated"})
        }
        "deactivate_task" => {
            // clear the active task pointer
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            engine
                .require_seat_manage(seat_id, target)
                .await
                .map_err(json_err)?;
            engine
                .seats
                .set_active_task(target, None, None)
                .await
                .map_err(json_err)?;
            json!({"success": true, "seat_id": target, "message": "Task deactivated"})
        }
        "get_active_task" => {
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            engine
                .require_seat_manage(seat_id, target)
                .await
                .map_err(json_err)?;
            match engine.task_get_active(target).await.map_err(json_err)? {
                Some(t) => {
                    json!({"success": true, "has_active_task": true, "target_seat": target, "task_id": t.task_id, "name": t.name, "description": t.description, "status": t.status, "project_id": t.project_id})
                }
                None => {
                    json!({"success": true, "has_active_task": false, "target_seat": target, "message": "No active task"})
                }
            }
        }
        "activate_document" => {
            let document_id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            let ok = engine
                .document_activate_for(seat_id, target, document_id)
                .await
                .map_err(json_err)?;
            if ok {
                // Hook: notify subscribers (SSE) so automation can react.
                let _ = events.send(json!({
                    "type": "document_activated", "seat_id": target, "document_id": document_id,
                }));
                json!({"success": true, "document_id": document_id, "target_seat": target, "message": "Document activated (context anchor)"})
            } else {
                json!({"success": false, "error": format!("document not found: {document_id}")})
            }
        }
        "deactivate_document" => {
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            engine
                .document_deactivate_for(seat_id, target)
                .await
                .map_err(json_err)?;
            json!({"success": true, "target_seat": target, "message": "Active document cleared"})
        }
        "get_active_document" => {
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            engine
                .require_seat_manage(seat_id, target)
                .await
                .map_err(json_err)?;
            match engine.document_get_active(target).await.map_err(json_err)? {
                Some(d) => {
                    json!({"success": true, "has_active_document": true, "target_seat": target, "document_id": d.document_id, "category": d.category.as_str(), "content": d.content, "tags": d.tags})
                }
                None => {
                    json!({"success": true, "has_active_document": false, "target_seat": target, "message": "No active document"})
                }
            }
        }
        "list_tasks" => {
            let project_id = args
                .get("project_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let status = args.get("status").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            if let Some(target) = args
                .get("target_seat")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                engine
                    .require_seat_manage(seat_id, target)
                    .await
                    .map_err(json_err)?;
                let tasks = engine
                    .task_list(target, project_id, status, limit)
                    .await
                    .map_err(json_err)?;
                json!({"success": true, "tasks": tasks, "count": tasks.len(), "target_seat": target, "scope": "legacy-seat"})
            } else {
                let scope = match args.get("scope").and_then(Value::as_str) {
                    Some(raw) => TaskListScope::parse(raw).ok_or_else(|| {
                        json!({"code": -32602, "message": format!("unsupported task scope: {raw}")})
                    })?,
                    None => TaskListScope::Visible,
                };
                let assignee = args.get("assignee").and_then(Value::as_str);
                let issuer = args.get("issuer").and_then(Value::as_str);
                let tasks = engine
                    .workflow_list_tasks(
                        seat_id, scope, status, project_id, assignee, issuer, limit,
                    )
                    .await
                    .map_err(json_err)?;
                json!({"success": true, "tasks": tasks, "count": tasks.len(), "principal": engine.workflow_principal(seat_id)})
            }
        }
        // projects
        "create_project" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let auto_load = str_array(args, "auto_load");
            let metadata = args.get("metadata").cloned().unwrap_or(json!({}));
            let p = engine
                .project_create(seat_id, name, description, &auto_load, &metadata)
                .await
                .map_err(json_err)?;
            json!({"success": true, "project_id": p.project_id, "name": p.name, "message": format!("Project '{}' created", p.name)})
        }
        "update_project" => {
            let project_id = args
                .get("project_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let name = args.get("name").and_then(|v| v.as_str());
            let description = args.get("description").and_then(|v| v.as_str());
            let description_patch = args
                .get("diff")
                .cloned()
                .or_else(|| args.get("description_patch").cloned());
            if description.is_some() && description_patch.is_some() {
                return Err(
                    json!({"code": -32602, "message": "pass either full-replacement description or diff/description_patch, not both"}),
                );
            }
            let auto_load = args.get("auto_load").map(|_| str_array(args, "auto_load"));
            let status = args.get("status").and_then(|v| v.as_str());
            let metadata = args.get("metadata").cloned();
            match engine
                .project_update(
                    seat_id,
                    project_id,
                    name,
                    description,
                    description_patch.as_ref(),
                    auto_load.as_deref(),
                    status,
                    metadata.as_ref(),
                )
                .await
                .map_err(json_err)?
            {
                Some(_) => {
                    let _ = engine.reembed_document(project_id).await;
                    json!({"success": true, "project_id": project_id, "message": "Project updated"})
                }
                None => {
                    json!({"success": false, "error": format!("Project not found: {project_id}")})
                }
            }
        }
        "delete_project" => {
            let project_id = args
                .get("project_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ok = engine
                .project_delete(seat_id, project_id)
                .await
                .map_err(json_err)?;
            json!({"success": ok, "project_id": project_id, "message": if ok { "Project deleted" } else { "Project not found" }})
        }
        "get_project" => {
            let project_id = args
                .get("project_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match engine
                .project_get(seat_id, project_id)
                .await
                .map_err(json_err)?
            {
                Some(p) => {
                    json!({"success": true, "project_id": p.project_id, "name": p.name, "description": p.description, "status": p.status, "auto_load": p.auto_load})
                }
                None => {
                    json!({"success": false, "error": format!("Project not found: {project_id}")})
                }
            }
        }
        "list_projects" => {
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            engine
                .require_seat_manage(seat_id, target)
                .await
                .map_err(json_err)?;
            let status = args.get("status").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let projects = engine
                .project_list(target, status, limit)
                .await
                .map_err(json_err)?;
            json!({"success": true, "projects": projects.iter().map(|p| json!({
                "project_id": p.project_id, "name": p.name, "status": p.status,
                "auto_load_count": p.auto_load.len(),
            })).collect::<Vec<_>>(), "count": projects.len(), "target_seat": target})
        }
        // profiles
        "get_user_profile" => match engine.get_user_profile(seat_id).await.map_err(json_err)? {
            Some(content) => json!({"exists": true, "content": content}),
            None => json!({"exists": false, "hint": "Create with update_user_profile(content)"}),
        },
        "update_user_profile" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let existed = engine
                .upsert_user_profile(seat_id, content)
                .await
                .map_err(json_err)?;
            json!({"success": true, "action": if existed { "updated" } else { "created" }})
        }
        "get_seat_profile" => match engine.get_seat_profile(seat_id).await.map_err(json_err)? {
            Some((content, tz)) => {
                json!({"exists": true, "seat_id": seat_id, "timezone": tz, "content": content})
            }
            None => json!({"exists": false, "hint": "Create with update_seat_profile(content)"}),
        },
        "update_seat_profile" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let timezone = args.get("timezone").and_then(|v| v.as_str());
            let existed = engine
                .upsert_seat_profile(seat_id, content, timezone)
                .await
                .map_err(json_err)?;
            json!({"success": true, "action": if existed { "updated" } else { "created" }, "seat_id": seat_id})
        }
        // pagination
        "get_page" => {
            let response_id = args
                .get("response_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let page = args.get("page").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            engine
                .get_page(seat_id, response_id, page)
                .await
                .map_err(json_err)?
        }
        "delete_response" => {
            let response_id = args
                .get("response_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            engine
                .delete_response(response_id)
                .await
                .map_err(json_err)?
        }
        "set_page_limit" => {
            let Some(tokens) = args
                .get("page_token_limit")
                .and_then(|v| v.as_u64())
                .map(|value| value as usize)
            else {
                return Err(json!({"code": -32602, "message": "page_token_limit is required"}));
            };
            engine.set_page_limit(tokens).await.map_err(json_err)?
        }
        "get_page_settings" => {
            let mut settings = engine.page_settings().await.map_err(json_err)?;
            let effective_limit = match pagination.page_token_limit {
                Some(limit) => limit,
                None => engine.page_token_limit().await.map_err(json_err)?,
            };
            settings["effective_enabled"] = json!(pagination.enabled);
            settings["effective_page_token_limit"] = json!(effective_limit);
            settings["connection_override"] = json!({
                "enabled": pagination.enabled
                    != slc_core::pagination::pagination_enabled_from_env(),
                "page_token_limit": pagination.page_token_limit.is_some(),
                "context_token_limit": pagination.context_token_limit,
            });
            settings
        }
        "update_context" => {
            let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let include_base = args
                .get("include_base_docs")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let changes = str_array(args, "changes");
            let decisions = str_array(args, "decisions");
            let next_steps = str_array(args, "next_steps");
            build_context(
                engine,
                seat_id,
                summary,
                &changes,
                &decisions,
                &next_steps,
                include_base,
                pagination.context_token_limit,
                events,
            )
            .await?
        }
        "save_context" => {
            let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            if summary.trim().is_empty() {
                return Err(json!({"code": -32602, "message": "summary is required"}));
            }
            let include_base = args
                .get("include_base_docs")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let changes = str_array(args, "changes");
            let decisions = str_array(args, "decisions");
            let next_steps = str_array(args, "next_steps");
            build_context(
                engine,
                seat_id,
                summary,
                &changes,
                &decisions,
                &next_steps,
                include_base,
                pagination.context_token_limit,
                events,
            )
            .await?
        }
        "load_module" => {
            // Requires knowledge:public:write; in embedded/legacy modes every
            // principal is a superuser, so accept. Module seeding is out of
            // scope for the standalone engine — report the known modules.
            let module = args
                .get("module_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let available = ["languages", "methodologies", "security"];
            if !available.contains(&module) {
                json!({"success": false, "error": format!("Unknown module: {module}"), "available_modules": available})
            } else {
                json!({"success": true, "module": module, "loaded_count": 0, "skipped_count": 0, "loaded_documents": []})
            }
        }
        "project_set_status" => {
            let project_id = args
                .get("project_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let status = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            match engine
                .project_set_status_for(seat_id, target, project_id, status)
                .await
                .map_err(json_err)?
            {
                Some(p) => {
                    json!({"success": true, "project_id": p.project_id, "status": p.status, "target_seat": target, "message": format!("Project status set to {}", p.status)})
                }
                None => {
                    json!({"success": false, "error": format!("project not found: {project_id}")})
                }
            }
        }
        "focus_set_archived" => {
            let focus_id = args.get("focus_id").and_then(|v| v.as_str()).unwrap_or("");
            let archived = args
                .get("archived")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let target = args
                .get("target_seat")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            let ok = engine
                .focus_set_archived_for(seat_id, target, focus_id, archived)
                .await
                .map_err(json_err)?;
            json!({"success": ok, "focus_id": focus_id, "archived": archived, "target_seat": target, "message": if ok { "Focus updated" } else { "Focus not found" }})
        }
        "seat_roles" => {
            let q = args
                .get("seat_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(seat_id);
            let roles: Vec<&str> = engine.seat_roles(q).iter().map(|r| r.as_str()).collect();
            json!({
                "success": true,
                "seat_id": q,
                "roles": roles,
                "can_manage_seats": engine.can_manage_seats(q),
                "allowed_targets": engine.allowed_manage_targets(q),
            })
        }
        "rename_document" => {
            let document_id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_id = args
                .get("new_document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let report = engine
                .rename_document(seat_id, document_id, new_id, None)
                .await
                .map_err(json_err)?;
            json!({"success": true, "old_id": report.old_id, "new_id": report.new_id,
                   "links_fixed": report.links_fixed, "content_links_fixed": report.content_links_fixed,
                   "seats_updated": report.seats_updated,
                   "message": format!("Renamed {} → {}", report.old_id, report.new_id)})
        }
        "rename_task" => {
            let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let new_name = args.get("new_name").and_then(|v| v.as_str()).unwrap_or("");
            let report = engine
                .rename_task(seat_id, task_id, new_name)
                .await
                .map_err(json_err)?;
            json!({"success": true, "old_id": report.old_id, "new_id": report.new_id,
                   "name": new_name, "links_fixed": report.links_fixed,
                   "seats_updated": report.seats_updated,
                   "message": format!("Task renamed: {} → {}", report.old_id, report.new_id)})
        }
        "rename_project" => {
            let project_id = args
                .get("project_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new_name = args.get("new_name").and_then(|v| v.as_str()).unwrap_or("");
            let report = engine
                .rename_project(seat_id, project_id, new_name)
                .await
                .map_err(json_err)?;
            json!({"success": true, "old_id": report.old_id, "new_id": report.new_id,
                   "name": new_name, "links_fixed": report.links_fixed,
                   "seats_updated": report.seats_updated,
                   "message": format!("Project renamed: {} → {}", report.old_id, report.new_id)})
        }
        "list_documents" => {
            let category = args
                .get("category")
                .and_then(|v| v.as_str())
                .and_then(DocumentCategory::parse);
            let folder = args
                .get("folder")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let query = args
                .get("query")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(100)
                .min(500) as usize;
            let filter = DocFilter {
                category,
                visible_to: Some(seat_id.into()),
                ..Default::default()
            };
            let docs = engine
                .store()
                .kb_find(&filter, &DocSort::by_updated(SortDir::Desc), limit)
                .await
                .map_err(json_err)?;
            let out: Vec<Value> = docs
                .iter()
                .filter(|d| folder.map_or(true, |f| d.folder.as_deref() == Some(f)))
                .filter(|d| {
                    query.map_or(true, |q| {
                        let q = q.to_lowercase();
                        d.document_id.to_lowercase().contains(&q)
                            || d.content.to_lowercase().contains(&q)
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
            json!({"success": true, "documents": out, "count": out.len()})
        }
        "update_document" => {
            let id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Some(mut doc) = engine.get_document(id).await.map_err(json_err)? else {
                return Err(
                    json!({"code": -32602, "message": format!("document not found: {id}")}),
                );
            };
            if slc_core::tasks::is_workflow_task(&doc) {
                return Err(json!({
                    "code": -32602,
                    "message": "workflow tasks are append-only; use task_message or report_task"
                }));
            }
            // Тело: content (legacy, полная замена) и/или diff — оба
            // принимаются (старые инструкции агентов).
            if let Some(c) = args.get("content").and_then(|v| v.as_str()) {
                doc.content = c.to_string();
                doc.content_hash = slc_core::content_hash(c);
            }
            if let Some(patch) = args.get("diff").cloned() {
                doc.content = slc_core::tasks::apply_description_patch(&doc.content, &patch)
                    .map_err(|e| json!({"code": -32602, "message": e}))?;
                doc.content_hash = slc_core::content_hash(&doc.content);
            }
            if let Some(t) = args.get("tags").and_then(|v| v.as_array()) {
                doc.tags = t
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect();
            }
            if let Some(al) = args.get("auto_load").and_then(|v| v.as_array()) {
                doc.auto_load = al
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect();
            }
            if let Some(r) = args.get("references").and_then(|v| v.as_array()) {
                doc.references = r
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect();
            }
            if let Some(m) = args.get("metadata").and_then(|v| v.as_object()) {
                for (k, v) in m {
                    doc.metadata.extra.insert(k.clone(), v.clone());
                }
            }
            if let Some(s) = args.get("seat_id").and_then(|v| v.as_str()) {
                doc.seat_id = if s.is_empty() { None } else { Some(s.into()) };
            }
            doc.updated_at = chrono::Utc::now();
            doc.version += 1;
            engine.store().kb_replace(&doc).await.map_err(json_err)?;
            let _ = engine.reembed_document(id).await;
            json!({"success": true, "document_id": id})
        }
        "delete_document" => {
            let id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let purge = args.get("purge").and_then(|v| v.as_bool()).unwrap_or(false);
            if let Some(doc) = engine.store().kb_get(id).await.map_err(json_err)?
                && slc_core::tasks::is_workflow_task(&doc)
            {
                return Err(json!({
                    "code": -32602,
                    "message": "workflow tasks cannot be deleted; preserve their event history"
                }));
            }
            let ok = if purge {
                engine.store().kb_purge(id).await.map_err(json_err)?
            } else {
                engine.store().kb_soft_delete(id).await.map_err(json_err)?
            };
            json!({"success": ok, "document_id": id, "purged": purge})
        }
        "state_get" => {
            let namespace = state_namespace(args)?;
            let key = state_key(args)?;
            let id = state_document_id(seat_id, namespace, key);
            match engine.store().kb_get(&id).await.map_err(json_err)? {
                Some(doc) if state_document_matches(&doc, seat_id, namespace, key) => json!({
                    "found": true,
                    "namespace": namespace,
                    "key": key,
                    "content": doc.content,
                    "content_type": doc.metadata.extra.get("external_state_content_type")
                        .and_then(Value::as_str)
                        .unwrap_or("text/plain; charset=utf-8"),
                    "etag": state_etag(&doc.content),
                    "version": doc.version,
                    "updated_at": doc.updated_at.to_rfc3339(),
                }),
                _ => json!({"found": false, "namespace": namespace, "key": key}),
            }
        }
        "state_put" => {
            let namespace = state_namespace(args)?;
            let key = state_key(args)?;
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            if content.chars().count() > STATE_MAX_CONTENT_CHARS {
                return Err(json!({
                    "code": -32602,
                    "message": format!("content exceeds {STATE_MAX_CONTENT_CHARS} characters")
                }));
            }
            let content_type = args
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("text/plain; charset=utf-8");
            let expected = args.get("expected_etag").and_then(Value::as_str);
            let id = state_document_id(seat_id, namespace, key);
            let existing = engine.store().kb_get(&id).await.map_err(json_err)?;
            if let Some(ref doc) = existing {
                if !state_document_matches(doc, seat_id, namespace, key) {
                    return Err(
                        json!({"code": -32009, "message": "external-state identity collision"}),
                    );
                }
            }
            let actual = existing.as_ref().map(|doc| state_etag(&doc.content));
            if let Some(expected) = expected {
                let matches = if expected.is_empty() {
                    actual.is_none()
                } else {
                    actual.as_deref() == Some(expected)
                };
                if !matches {
                    return Err(json!({
                        "code": -32009,
                        "message": "external-state etag conflict",
                        "expected_etag": expected,
                        "actual_etag": actual,
                    }));
                }
            }

            let mut metadata = DocMeta::default();
            metadata
                .extra
                .insert("external_state_namespace".into(), json!(namespace));
            metadata
                .extra
                .insert("external_state_key".into(), json!(key));
            metadata
                .extra
                .insert("external_state_content_type".into(), json!(content_type));
            let seat_hash = slc_core::content_hash(seat_id);
            let mut doc = Document::with_folder(
                &id,
                if namespace.contains("skill") {
                    DocumentCategory::Skill
                } else {
                    DocumentCategory::Custom
                },
                Some(format!("external-state/{}/{}", &seat_hash[..16], namespace)),
                content,
                metadata,
                vec!["external-state".into(), state_namespace_tag(namespace)],
                Some(seat_id.into()),
            );
            if let Some(previous) = existing {
                doc.created_at = previous.created_at;
                doc.version = previous.version + 1;
            }
            engine.store().kb_replace(&doc).await.map_err(json_err)?;
            json!({
                "success": true,
                "created": actual.is_none(),
                "namespace": namespace,
                "key": key,
                "etag": state_etag(content),
                "version": doc.version,
            })
        }
        "state_list" => {
            let namespace = state_namespace(args)?;
            let prefix = args.get("prefix").and_then(Value::as_str).unwrap_or("");
            if prefix.chars().count() > 512 || prefix.contains('\0') {
                return Err(
                    json!({"code": -32602, "message": "prefix exceeds 512 characters or contains NUL"}),
                );
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(1000)
                .clamp(1, 1000) as usize;
            let docs = engine
                .store()
                .kb_find(
                    &DocFilter {
                        seat_id: Some(seat_id.into()),
                        tags_all: vec!["external-state".into(), state_namespace_tag(namespace)],
                        ..Default::default()
                    },
                    &DocSort::by_updated(SortDir::Desc),
                    1000,
                )
                .await
                .map_err(json_err)?;
            let objects = docs
                .iter()
                .filter_map(|doc| {
                    let key = doc
                        .metadata
                        .extra
                        .get("external_state_key")
                        .and_then(Value::as_str)?;
                    if !key.starts_with(prefix) {
                        return None;
                    }
                    Some(json!({
                        "key": key,
                        "content_type": doc.metadata.extra.get("external_state_content_type")
                            .and_then(Value::as_str)
                            .unwrap_or("text/plain; charset=utf-8"),
                        "etag": state_etag(&doc.content),
                        "version": doc.version,
                        "size_chars": doc.content.chars().count(),
                        "updated_at": doc.updated_at.to_rfc3339(),
                    }))
                })
                .take(limit)
                .collect::<Vec<_>>();
            json!({
                "success": true,
                "namespace": namespace,
                "prefix": prefix,
                "count": objects.len(),
                "objects": objects,
            })
        }
        "state_delete" => {
            let namespace = state_namespace(args)?;
            let key = state_key(args)?;
            let id = state_document_id(seat_id, namespace, key);
            let existing = engine.store().kb_get(&id).await.map_err(json_err)?;
            if let Some(doc) = existing {
                if !state_document_matches(&doc, seat_id, namespace, key) {
                    return Err(
                        json!({"code": -32009, "message": "external-state identity collision"}),
                    );
                }
                if let Some(expected) = args.get("expected_etag").and_then(Value::as_str) {
                    let actual = state_etag(&doc.content);
                    if expected != actual {
                        return Err(json!({
                            "code": -32009,
                            "message": "external-state etag conflict",
                            "expected_etag": expected,
                            "actual_etag": actual,
                        }));
                    }
                }
                let deleted = engine.store().kb_purge(&id).await.map_err(json_err)?;
                let _ = engine.store().delete_embeddings(&id).await;
                json!({
                    "success": true,
                    "deleted": deleted,
                    "namespace": namespace,
                    "key": key,
                })
            } else {
                json!({
                    "success": true,
                    "deleted": false,
                    "namespace": namespace,
                    "key": key,
                })
            }
        }
        "document_stats" => {
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
                        visible_to: Some(seat_id.into()),
                        ..Default::default()
                    })
                    .await
                    .map_err(json_err)?;
                by_cat.insert(cat.as_str().into(), json!(n));
            }
            let total = engine
                .store()
                .kb_count(&DocFilter {
                    visible_to: Some(seat_id.into()),
                    ..Default::default()
                })
                .await
                .map_err(json_err)?;
            let episodic = engine
                .store()
                .episodic_count(&DocFilter::default())
                .await
                .map_err(json_err)?;
            let seats = engine
                .seats
                .list_active(1000)
                .await
                .map_err(json_err)?
                .len();
            json!({
                "total": total,
                "by_category": by_cat,
                "episodic": episodic,
                "seats": seats,
                "tasks": by_cat.get("task").cloned().unwrap_or(json!(0)),
                "projects": by_cat.get("project").cloned().unwrap_or(json!(0)),
            })
        }
        "list_seats" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let seats = engine
                .seats
                .list_active(limit)
                .await
                .map_err(json_err)?
                .into_iter()
                .filter(|seat| engine.can_manage_target(seat_id, &seat.seat_id))
                .collect::<Vec<_>>();
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
        }
        "notification_list" => {
            let status = args.get("status").and_then(|v| v.as_str());
            let notes = engine
                .list_notifications(seat_id, status)
                .await
                .map_err(json_err)?;
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
        }
        _ => return Err(json!({"code": -32601, "message": format!("unknown tool: {name}")})),
    };
    let _ = engine.seats.record_tool_use(seat_id, name, 0).await;
    let mut text = text.to_string();
    // Inject pending notifications into the tool response (UX channel).
    // Machine-oriented external-state operations must remain parseable JSON.
    // Human notification prose is useful on interactive knowledge tools, but
    // appending it would corrupt the generic state backend contract for every
    // MCP client, not just Hermes.
    let tool_text = text.clone();
    if name != "pop_notifications" && !name.starts_with("state_") {
        if let Ok(notes) = engine.pop_notifications(seat_id, 3).await {
            if !notes.is_empty() {
                let mut block = String::from("\n\n📬 Notifications:");
                for n in notes {
                    block.push_str(&format!("\n- [{}] {}", n.source, n.body));
                }
                text.push_str(&block);
            }
        }
    }
    let mut result = json!({"content": [{"type": "text", "text": text}], "isError": false});
    // structuredContent: некоторые клиенты требуют его, когда у тула есть
    // outputSchema. Схем у наших тулов нет, поэтому для БОЛЬШИХ ответов
    // дубль не отдаём — он удваивал вывод (text + structuredContent) и
    // выводил за resultBudget харнеса (обрезка strategy=truncate).
    if tool_text.chars().count() <= 2000 {
        if let Ok(parsed) = serde_json::from_str::<Value>(&tool_text) {
            result["structuredContent"] = parsed;
        } else {
            result["structuredContent"] = json!({"text": tool_text});
        }
    }
    // Pagination envelope: cache oversized list responses and tag them.
    // Paginate the inner JSON value rather than the MCP ContentBlock array;
    // otherwise `content` itself looks like the list and no useful split is
    // possible. This is an application-level SLC tool contract layered on
    // ordinary MCP TextContent, so it remains consumable by any MCP client.
    let pagination_candidate = !matches!(
        name,
        "get_page" | "delete_response" | "set_page_limit" | "get_page_settings"
    );
    let effective_page_token_limit = match pagination.page_token_limit {
        Some(limit) => limit,
        None => engine.page_token_limit().await.map_err(json_err)?,
    };
    if pagination.enabled && pagination_candidate {
        let pagination_threshold =
            effective_page_token_limit.saturating_mul(slc_core::pagination::CHARS_PER_TOKEN);
        if tool_text.chars().count() > pagination_threshold {
            if let Ok(parsed) = serde_json::from_str::<Value>(&tool_text) {
                let response_id = uid("resp");
                let paginated = engine
                    .paginate_with_limit(seat_id, &response_id, &parsed, effective_page_token_limit)
                    .await
                    .map_err(json_err)?;
                if let Some(page) = paginated.get("_pagination") {
                    result["_pagination"] = page.clone();
                    // The first page contains only whole items. Remaining
                    // items are available through the advertised get_page
                    // tool without relying on a client-side patch.
                    if let Ok(page_text) = serde_json::to_string(&paginated) {
                        let notification_suffix = text
                            .strip_prefix(&tool_text)
                            .unwrap_or_default()
                            .to_string();
                        text = page_text;
                        // Give every client an explicit, tool-level recovery
                        // path for the remaining pages.
                        if let (Some(pid), Some(total)) = (
                            page.get("response_id").and_then(|v| v.as_str()),
                            page.get("total_pages").and_then(|v| v.as_u64()),
                        ) {
                            if total > 1 {
                                text.push_str(&format!(
                                    "\n\n📄 Paginated response: page 1 of {total}.\n"
                                ));
                                text.push_str(&format!(
                                    "Retrieve every remaining page in order with get_page(response_id={pid}, page=2..{total}), one page per call, and combine the items.\n"
                                ));
                                text.push_str("Pages may contain PARTS of one large document (field `part: \"k/n\"`) — concatenate them in part order to get the full content.\n");
                                text.push_str("Do not finish processing the response until all pages have been retrieved.");
                            }
                        }
                        text.push_str(&notification_suffix);
                        result["content"][0]["text"] = json!(text);
                    }
                }
            }
        } else if tool_text.chars().count() > 16_000 {
            // Ответ большой, но пагинация не сработала (лимит страницы
            // велик или пагинация выключена) — клиент, скорее всего,
            // обрежет выхлоп. Явно подсказываем, что делать.
            text.push_str(&format!(
                    "\n\n⚠️ Ответ большой ({} символов) и НЕ пагинирован (лимит страницы {} токенов ≈ {} символов). Если клиент обрезает выхлоп: set_page_limit(<{}>) и повтори вызов тула.",
                    tool_text.chars().count(),
                    effective_page_token_limit,
                    effective_page_token_limit.saturating_mul(slc_core::pagination::CHARS_PER_TOKEN),
                    (tool_text.chars().count() / slc_core::pagination::CHARS_PER_TOKEN).max(1),
                ));
            result["content"][0]["text"] = json!(text);
        }
    }
    Ok(result)
}

fn json_err(e: slc_core::SlcError) -> Value {
    json!({"code": -32000, "message": e.to_string()})
}

fn parse_mind(s: &str) -> Option<slc_core::MindType> {
    slc_core::MindType::parse(s).ok()
}

/// Read a `["a","b"]` argument as `Vec<String>`.
fn str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// A short unique id (no slc_core dependency for the server).
fn uid(prefix: &str) -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("{prefix}_{}", &u[..12])
}

/// SSE fan-out filter: forward `evt` only to the subscriber of `seat`.
/// Deny-by-default — an event WITHOUT a `seat_id` goes to nobody: it carries
/// no routing context, and broadcasting it to every subscriber would leak
/// (e.g. a sampling prompt with document contents).
fn seat_matches_event(evt: &serde_json::Value, seat: &str) -> bool {
    evt.get("seat_id")
        .and_then(|v| v.as_str())
        .map(|s| s == seat)
        .unwrap_or(false)
}

/// Convert an internally routed event into the SSE shape consumed by MCP
/// clients. Sampling must leave the server as a raw JSON-RPC request; wrapping
/// it in `{type, seat_id, message}` makes standard SDKs treat it as an unknown
/// notification and the reasoning call times out.
fn sse_delivery(evt: &Value) -> Option<(&'static str, Value)> {
    match evt.get("type").and_then(Value::as_str) {
        Some("rpc_response") => evt
            .get("response")
            .cloned()
            .map(|payload| ("message", payload)),
        Some("sampling_request") => evt
            .get("message")
            .cloned()
            .map(|payload| ("message", payload)),
        _ => Some(("notification", evt.clone())),
    }
}

fn ping_result() -> Value {
    json!({})
}

#[cfg(test)]
mod seat_filter_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn seat_filter_is_deny_by_default() {
        // Same seat → delivered.
        assert!(seat_matches_event(
            &json!({"type": "x", "seat_id": "cursor-1"}),
            "cursor-1"
        ));
        // Other seat → withheld.
        assert!(!seat_matches_event(
            &json!({"type": "x", "seat_id": "cursor-1"}),
            "cursor-2"
        ));
        // NO seat_id at all → withheld (regression: it used to be broadcast
        // to every subscriber — sampling prompts leaked across seats).
        assert!(!seat_matches_event(
            &json!({"type": "sampling_request"}),
            "cursor-1"
        ));
        // Malformed seat → withheld.
        assert!(!seat_matches_event(
            &json!({"type": "x", "seat_id": 42}),
            "cursor-1"
        ));
    }

    #[test]
    fn sampling_is_delivered_as_raw_jsonrpc_message() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "smp-1",
            "method": "sampling/createMessage",
            "params": {"messages": [], "maxTokens": 2000}
        });
        let routed = json!({
            "type": "sampling_request",
            "seat_id": "junior-1",
            "message": request,
        });
        let (event_name, payload) = sse_delivery(&routed).unwrap();
        assert_eq!(event_name, "message");
        assert_eq!(payload, request);
        assert!(payload.get("seat_id").is_none());
    }

    #[test]
    fn sampling_response_accepts_current_and_legacy_content_shapes() {
        let current = json!({
            "jsonrpc": "2.0",
            "id": "smp-1",
            "result": {
                "role": "assistant",
                "content": {"type": "text", "text": "project-a"},
                "model": "test"
            }
        });
        assert_eq!(
            sampling_response_text(&current).as_deref(),
            Some("project-a")
        );

        let legacy = json!({
            "jsonrpc": "2.0",
            "id": "smp-2",
            "result": {"content": [{"type": "text", "text": "project-b"}]}
        });
        assert_eq!(
            sampling_response_text(&legacy).as_deref(),
            Some("project-b")
        );
    }

    #[test]
    fn ping_result_is_an_mcp_response_object() {
        assert_eq!(ping_result(), json!({}));
        assert!(ping_result().is_object());
    }

    #[test]
    fn only_initialize_requests_create_streamable_http_sessions() {
        assert!(is_initialize_request(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        })));
        assert!(!is_initialize_request(&json!({
            "jsonrpc": "2.0",
            "method": "initialize",
            "params": {}
        })));
        assert!(!is_initialize_request(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        })));
    }

    #[test]
    fn pagination_transport_overrides_are_connection_scoped() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-slc-pagination", "disabled".parse().unwrap());
        headers.insert("x-slc-page-token-limit", "25000".parse().unwrap());
        headers.insert("x-slc-context-token-limit", "100000".parse().unwrap());
        let policy = pagination_policy_from_request(&headers);
        assert!(!policy.enabled);
        assert_eq!(policy.page_token_limit, Some(25_000));
        assert_eq!(policy.context_token_limit, Some(100_000));

        assert_eq!(
            effective_context_token_limit(Some(300_000), 100_000),
            300_000
        );
        assert_eq!(effective_context_token_limit(None, 100_000), 100_000);

        headers.insert("x-slc-pagination", "enabled".parse().unwrap());
        headers.insert("x-slc-page-token-limit", "350000".parse().unwrap());
        let policy = pagination_policy_from_request(&headers);
        assert!(policy.enabled);
        assert_eq!(policy.page_token_limit, Some(350_000));
    }

    #[test]
    fn jsonrpc_notifications_are_distinguished_from_requests_and_responses() {
        assert!(is_jsonrpc_notification(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })));
        assert!(is_jsonrpc_notification(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": 7}
        })));
        assert!(is_jsonrpc_notification(&json!({
            "jsonrpc": "2.0",
            "method": "vendor/custom-notification"
        })));
        assert!(!is_jsonrpc_notification(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping"
        })));
        assert!(!is_jsonrpc_notification(&json!({
            "jsonrpc": "2.0",
            "id": "sampling-1",
            "result": {"content": []}
        })));
    }

    #[test]
    fn external_state_identity_is_seat_and_namespace_scoped() {
        let first = state_document_id("seat-a", "hermes-memory", "MEMORY.md");
        assert_eq!(
            first,
            state_document_id("seat-a", "hermes-memory", "MEMORY.md")
        );
        assert_ne!(
            first,
            state_document_id("seat-b", "hermes-memory", "MEMORY.md")
        );
        assert_ne!(
            first,
            state_document_id("seat-a", "hermes-skills", "MEMORY.md")
        );
    }

    #[test]
    fn external_state_namespace_validation_is_strict() {
        assert_eq!(
            state_namespace(&json!({"namespace": "hermes-skills.v1"})).unwrap(),
            "hermes-skills.v1"
        );
        assert!(state_namespace(&json!({"namespace": "../escape"})).is_err());
        assert!(state_namespace(&json!({"namespace": ""})).is_err());
    }

    #[test]
    fn workflow_delivery_is_content_free_and_transport_neutral() {
        let delivery = workflow_delivery(
            Some("dev-junior-0"),
            "task_example",
            Some("event_example"),
            "message",
        );

        assert_eq!(delivery["recipient"], "dev-junior-0");
        assert_eq!(delivery["correlation_id"], "task_example");
        assert_eq!(delivery["idempotency_key"], "event_example");
        assert_eq!(delivery.as_object().unwrap().len(), 4);
        let serialized = delivery.to_string();
        assert!(!serialized.contains("canonical task body"));
        assert!(!serialized.contains("private report"));
        assert!(serialized.contains("list_task_events"));
    }
}
