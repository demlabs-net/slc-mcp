//! MCP server: JSON-RPC 2.0 over HTTP (`POST /mcp`) + streamable-HTTP SSE
//! (`GET /sse` → `POST /messages`) + `GET /health`. Tools are the canonical
//! snake_case catalog. Auth modes: `legacy_seat_id` (default),
//! `bearer_plus_seat`, `embedded` (via `SLC_MCP_AUTH`).

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde_json::{Value, json};
use slc_core::{DocFilter, DocMeta, DocSort, Document, DocumentCategory, SlcEngine, SortDir};
use std::sync::Arc;

pub struct AppState {
    pub engine: Arc<SlcEngine>,
    /// Server→client notification fan-out keyed by seat id.
    pub events: tokio::sync::broadcast::Sender<Value>,
    /// Pending MCP sampling requests (id → answer channel); the sampling
    /// LlmClient registers here and the client's response resolves it.
    pub sampling: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<String>>>,
    >,
}

pub async fn run(
    engine: SlcEngine,
    port: u16,
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
    });
    let app = Router::new()
        .route("/mcp", post(mcp))
        .route("/sse", get(sse_endpoint))
        .route("/messages", post(messages))
        .route("/health", get(health))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("SLC MCP listening on http://{addr}/mcp (SSE: /sse → /messages)");
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
                        // JSON-RPC replies (legacy SSE protocol) → `message`
                        // event with the response as data; everything else is
                        // a server notification.
                        if evt.get("type").and_then(|v| v.as_str()) == Some("rpc_response") {
                            if let Some(resp) = evt.get("response") {
                                yield Ok(Event::default().event("message").data(resp.to_string()));
                            }
                        } else {
                            yield Ok(Event::default().event("notification").data(evt.to_string()));
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
    let sid = headers
        .get("sessionId")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let seat = seat_from_request(&headers).unwrap_or_default();
    let (status, Json(body)) = mcp(State(state.clone()), headers, Json(req)).await;
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

#[axum::debug_handler]
async fn mcp(
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
                let text = req
                    .get("result")
                    .and_then(|r| r.get("content"))
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .and_then(|m| m.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = tx.try_send(text);
                return (StatusCode::OK, Json(json!({})));
            }
        }
    }

    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let engine = state.engine.as_ref();
    let seat_hdr = seat_from_request(&headers);
    let bearer = bearer_from_request(&headers);

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
        "initialize" => {
            // Auto-apply the client's context budget when it advertises one
            // (capabilities.experimental.context_limit_chars — ТОКЕНЫ) as the
            // per-seat override used by update_context compression.
            if let Some(window) = params
                .pointer("/capabilities/experimental/context_limit_chars")
                .and_then(|v| v.as_u64())
            {
                if let Some(seat) = seat_hdr.as_deref() {
                    // The seat may not exist yet — create it first.
                    let _ = engine.seats.ensure_seat(seat).await;
                    // SLC budget = 80% of the client's model window in TOKENS
                    // (the rest is left for the conversation itself), capped
                    // at the configured limit (SLC_CONTEXT_LIMIT_TOKENS).
                    let limit = (window * 4 / 5).min(engine.config.context_limit_tokens as u64);
                    let _ = engine
                        .seats
                        .set_context_key(seat, "context_limit_tokens", json!(limit))
                        .await;
                }
            }
            Ok(json!({
                "protocolVersion": "2025-03-26",
                "capabilities": { "tools": {}, "prompts": {} },
                "serverInfo": { "name": "slc-mcp", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(Value::Null),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "prompts/list" => Ok(json!({ "prompts": prompts() })),
        "prompts/get" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            match name {
                "instructions" => Ok(json!({
                    "name": "instructions",
                    "description": "Полная рабочая инструкция агента: рабочий процесс, auto_load vs references, рефлексия через истории",
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
            call_tool(engine, &seat, name, &args, &state.events).await
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

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Hybrid search over the knowledge base (semantic + BM25)",
            "inputSchema": {"type":"object","properties":{
                "query": {"type":"string","description":"search query"},
                "limit": {"type":"number","default":10}
            },"required":["query"]}
        }),
        json!({
            "name": "get_document",
            "description": "Load a document by its unique name id",
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
            "description": "List active focus items for the seat",
            "inputSchema": {"type":"object","properties":{
                "mind_type": {"type":"string","enum":["front","planner","executor","critic","shared"]}
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
            "name": "create_task",
            "description": "Create a new task (private to current seat)",
            "inputSchema": {"type":"object","properties":{
                "name": {"type":"string"},
                "description": {"type":"string","default":""},
                "project_id": {"type":"string"},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "metadata": {"type":"object"}
            },"required":["name"]}
        }),
        json!({
            "name": "update_task",
            "description": "Update an existing task",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"},
                "name": {"type":"string"},
                "description": {"type":"string"},
                "project_id": {"type":"string"},
                "auto_load": {"type":"array","items":{"type":"string"}},
                "status": {"type":"string","enum":["pending","active","completed","cancelled"]},
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
            "description": "Activate a task (included in update_context)",
            "inputSchema": {"type":"object","properties":{
                "task_id": {"type":"string"}
            },"required":["task_id"]}
        }),
        json!({
            "name": "deactivate_task",
            "description": "Deactivate the current active task",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "get_active_task",
            "description": "Get the currently active task",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "activate_document",
            "description": "Activate ANY document (task, project, skill, knowledge doc…) as the seat's context anchor — it is included in update_context and its auto_load links are followed on updates. The effect is identical to activate_task, but for every category.",
            "inputSchema": {"type":"object","properties":{
                "document_id": {"type":"string"}
            },"required":["document_id"]}
        }),
        json!({
            "name": "deactivate_document",
            "description": "Clear the seat's active document (any category)",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "get_active_document",
            "description": "Get the currently active document (any category)",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        json!({
            "name": "list_tasks",
            "description": "List your tasks with optional filters",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"},
                "status": {"type":"string","enum":["pending","active","completed","cancelled"]},
                "limit": {"type":"number","default":50}
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
            "description": "Update an existing project",
            "inputSchema": {"type":"object","properties":{
                "project_id": {"type":"string"},
                "name": {"type":"string"},
                "description": {"type":"string"},
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
            "description": "List all projects",
            "inputSchema": {"type":"object","properties":{
                "status": {"type":"string","enum":["active","archived"]},
                "limit": {"type":"number","default":50}
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
            "description": "Set the maximum page size (in tokens) for response pagination",
            "inputSchema": {"type":"object","properties":{
                "page_token_limit": {"type":"number"}
            },"required":["page_token_limit"]}
        }),
        json!({
            "name": "get_page_settings",
            "description": "Get current pagination settings",
            "inputSchema": {"type":"object","properties":{},"required":[]}
        }),
        // context
        json!({
            "name": "update_context",
            "description": "Load (and optionally save) project context: base docs + active task + profiles + focuses",
            "inputSchema": {"type":"object","properties":{
                "summary": {"type":"string","description":"persists a context snapshot when provided"},
                "changes": {"type":"array","items":{"type":"string"}},
                "decisions": {"type":"array","items":{"type":"string"}},
                "next_steps": {"type":"array","items":{"type":"string"}},
                "include_base_docs": {"type":"boolean","default":true}
            },"required":[]}
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
            "description": "Slash-команды для управления памятью: `/limit N` — лимит контекста, `/ctx` — текущий срез, `/update_context [summary]` / `/save_context <summary>` — как MCP-тулы, `/help` — список. Вызывай, когда пользователь пишет сообщение, начинающееся с '/'.",
            "inputSchema": {"type":"object","properties":{
                "input": {"type":"string","description":"строка, начинающаяся с /"}
            },"required":["input"]}
        }),
    ]
}

fn prompts() -> Vec<Value> {
    vec![
        json!({
            "name": "instructions",
            "description": "Полная рабочая инструкция агента: рабочий процесс, auto_load vs references, рефлексия через истории",
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
pub const INSTRUCTIONS_PROMPT: &str = r#"# SLC Memory — рабочая инструкция агента

Ты работаешь с системой памяти SLC (Smart Layered Context): единая база
документов (проекты, задачи, скилы, знания), эпизодическая история с
прогрессивной суммаризацией, фокусы, напоминания.

## Рабочий процесс (обязателен)

1. **Ищи перед тем, как отвечать.** На любой вопрос сначала `search` по
   базе знаний; если есть активный документ — начни с него (`get_document`
   по `get_active_document`). Не отвечай по памяти — SLC помнит за тебя.
2. **Активируй контекст.** Когда работа идёт над проектом/задачей/скилом —
   `activate_document` — документ станет контекст-якорем сита и будет
   включён в `update_context`. Активируй один главный документ, не
   несколько.
3. **Веди работу документами.** Новая деятельность → `create_task` (или
   `add_document` с category=task) и/или проект. Скилы — это тоже
   документы (category=skill): инструкции «как делать X».
4. **Обогащай по ходу.** После значимых шагов обновляй документы
   (`add_document` с тем же document_id — upsert): статусы, решения,
   новые факты. История (remember) — это сырьё, а документы — рабочий
   артефакт.
5. **auto_load vs references (важно, не путай).**
   - `auto_load` — РАБОЧИЕ связи: документы, которые должны подтягиваться
     в контекст при обновлении этого документа (состав, зависимости,
     связанные задачи). Ставь сюда то, что нужно видеть вместе.
   - `references` — ПАССИВНЫЕ упоминания: документы, на которые этот
     документ ссылается, но которые не нужны в контексте автоматически.
   Не дублируй одно и то же в оба списка.
6. **Контекст.** `update_context` возвращает текущий срез (базовые
   документы + активный документ + профили + фокусы). Сохраняй снимок
   (summary) в конце крупного этапа — это попадёт в историю.

## Рефлексия (это работа над историями, а не отдельный режим)

Периодически, когда накопилась работа (или пользователь просит
«порефлексируй»), проведи рефлексию через тулы:

1. `recall` — возьми свежую историю сита (10–30 событий); при
   необходимости `compress` (L1→L2) и `consolidate` (извлечение фактов),
   чтобы увидеть сжатую картину.
2. **Извлеки данные для документов.** Из историй вытащи: факты о
   пользователе, решения, договорённости, проблемы, метрики. Обогащай
   существующие документы (`add_document` с тем же id) или создавай новые
   (проект/задача/скил/знание), если темы ещё нет.
3. **Сделай ревью документов:** проверь актуальность (устаревшие статусы,
   факты, связи), обнови содержимое, добавь недостающие связи
   (auto_load/references), удали дубли.
4. **Обнови фокусы:** `focus_add`/`focus_update` — что реально важно
   сейчас; `focus_remove` — что закрыто.
5. **Результат рефлексии** — это изменённые/новые документы и фокусы, а
   не текст «я порефлексировал». Если менять нечего — так и скажи кратко.

## Лимиты и компрессия контекста

- `update_context` возвращает `limit_tokens`/`used_tokens`/`compressed` и,
  при сжатии, `warning`. Бюджет SLC — ТОКЕНЫ: 80% окна модели клиента (авто
  при подключении) или `/limit <tokens>`.
- **Документы НИКОГДА не обрезаются.** Если лимит превышен, компрессия:
  1) исключаются целые блоки по приоритету (base-документы, затем
     профили; активный документ и фокусы остаются);
  2) если всё ещё не влезает — оставшиеся документы интеллектуально
     сжимаются LLM (ключевые факты, имена, цифры).
- При `compressed: true` реагируй на `warning`: сокращай ответы, сохраняй
  снимок (`/save_context`), при необходимости верни исключённые документы
  через поиск/активацию.

## Хуки для кодинг-агента (автоматизация контекста)

Работая над кодом/задачами, автоматически:

1. **`update_context`** — после каждого значимого шага (закрыл задачу,
   изменил план, принял решение): контекст всегда актуален.
2. **`/save_context <summary>`** — в конце крупного этапа: снимок уходит
   в историю для рефлексии и консолидации.
3. **`activate_document`** — при смене темы работы (новая задача/проект/
   скил) сразу переключай контекст-якорь.
4. Сервер шлёт по SSE-каналу события `context_updated` и
   `document_activated` — обёртки агента могут слушать их для триггеров
   (например, авто-сохранение контекста после больших изменений).

## Напоминания

`check_notifications` в начале каждого хода — там могут быть фокус- или
таймер-напоминания, требующие действий.
"#;

async fn build_context(
    engine: &SlcEngine,
    seat_id: &str,
    summary: &str,
    changes: &[String],
    decisions: &[String],
    next_steps: &[String],
    include_base: bool,
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
    let limit_tokens = engine.context_limit_for(seat_id).await.map_err(json_err)?;
    let mut docs: Vec<Value> = Vec::new();
    // ЕДИНАЯ единица бюджета — ТОКЕНЫ (~3 симв/токен, RU/EN смесь).
    // Никаких байтовых ограничений вывода: размер окна определяет клиент
    // (initialize / /limit), а об обрезке на своей стороне заботится харнес.
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
            parts.push(format!("исключены целые блоки: {}", omitted.join(", ")));
        }
        if !llm_compressed.is_empty() {
            parts.push(format!(
                "документы сжаты LLM: {}",
                llm_compressed.join(", ")
            ));
        }
        Some(format!(
            "ВНИМАНИЕ: контекст сжат — {}.                      Используй save_context/обогащение, чтобы вернуть нужное.",
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

async fn call_tool(
    engine: &SlcEngine,
    seat_id: &str,
    name: &str,
    args: &Value,
    events: &tokio::sync::broadcast::Sender<Value>,
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
                        engine, seat_id, &summary, &empty, &empty, &empty, true, events,
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
                        engine, seat_id, &summary, &empty, &empty, &empty, true, events,
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
                        "snippet": h.document.content.chars().take(200).collect::<String>(),
                    })).collect::<Vec<_>>()}))
                }
                "help" => Ok(json!({"commands": [
                    "/limit <chars> — установить лимит контекста (символы)",
                    "/ctx — показать текущий срез контекста (лимит, активный документ, проекты, задачи, фокусы)",
                    "/search <query> — поиск по документам БЗ",
                    "/update_context [summary] — собрать контекст (как MCP-тул)",
                    "/save_context <summary> — сохранить снимок контекста в историю",
                    "/help — этот список",
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
                "content": truncate(&h.document.content, 2000),
            })).collect::<Vec<_>>()})
        }
        "get_document" => {
            let id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match engine.get_document(id).await.map_err(json_err)? {
                Some(d) => {
                    json!({"document_id": d.document_id, "category": d.category.as_str(), "content": d.content, "tags": d.tags, "metadata": d.metadata})
                }
                None => json!({"error": format!("not found: {id}")}),
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
                "content": truncate(&d.content, 1000),
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
            let mind_type = args
                .get("mind_type")
                .and_then(|v| v.as_str())
                .and_then(parse_mind);
            let items = engine
                .focus_list(seat_id, mind_type)
                .await
                .map_err(json_err)?;
            json!({"focuses": items.iter().map(|f| json!({
                "focus_id": f.focus_id, "title": f.title, "description": f.description,
                "priority": f.priority, "depends_on": f.depends_on,
            })).collect::<Vec<_>>()})
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
        "update_task" => {
            let task_id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = args.get("name").and_then(|v| v.as_str());
            let description = args.get("description").and_then(|v| v.as_str());
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
                    project_id,
                    auto_load.as_deref(),
                    status,
                    metadata.as_ref(),
                )
                .await
                .map_err(json_err)?
            {
                Some(_) => json!({"success": true, "task_id": task_id, "message": "Task updated"}),
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
            let ok = engine
                .task_activate(seat_id, task_id)
                .await
                .map_err(json_err)?;
            json!({"success": ok, "task_id": task_id, "message": "Task activated"})
        }
        "deactivate_task" => {
            // clear the active task pointer
            engine
                .seats
                .set_active_task(seat_id, None, None)
                .await
                .map_err(json_err)?;
            json!({"success": true, "seat_id": seat_id, "message": "Task deactivated"})
        }
        "get_active_task" => match engine.task_get_active(seat_id).await.map_err(json_err)? {
            Some(t) => {
                json!({"success": true, "has_active_task": true, "task_id": t.task_id, "name": t.name, "description": t.description, "status": t.status, "project_id": t.project_id})
            }
            None => json!({"success": true, "has_active_task": false, "message": "No active task"}),
        },
        "activate_document" => {
            let document_id = args
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let ok = engine
                .document_activate(seat_id, document_id)
                .await
                .map_err(json_err)?;
            if ok {
                // Hook: notify subscribers (SSE) so automation can react.
                let _ = events.send(json!({
                    "type": "document_activated", "seat_id": seat_id, "document_id": document_id,
                }));
                json!({"success": true, "document_id": document_id, "message": "Document activated (context anchor)"})
            } else {
                json!({"success": false, "error": format!("document not found: {document_id}")})
            }
        }
        "deactivate_document" => {
            engine
                .document_deactivate(seat_id)
                .await
                .map_err(json_err)?;
            json!({"success": true, "message": "Active document cleared"})
        }
        "get_active_document" => {
            match engine
                .document_get_active(seat_id)
                .await
                .map_err(json_err)?
            {
                Some(d) => {
                    json!({"success": true, "has_active_document": true, "document_id": d.document_id, "category": d.category.as_str(), "content": truncate(&d.content, 2000), "tags": d.tags})
                }
                None => {
                    json!({"success": true, "has_active_document": false, "message": "No active document"})
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
            let tasks = engine
                .task_list(seat_id, project_id, status, limit)
                .await
                .map_err(json_err)?;
            json!({"success": true, "tasks": tasks.iter().map(|t| json!({
                "task_id": t.task_id, "name": t.name, "status": t.status,
                "project_id": t.project_id, "auto_load_count": t.auto_load.len(),
            })).collect::<Vec<_>>(), "count": tasks.len()})
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
            let auto_load = args.get("auto_load").map(|_| str_array(args, "auto_load"));
            let status = args.get("status").and_then(|v| v.as_str());
            let metadata = args.get("metadata").cloned();
            match engine
                .project_update(
                    seat_id,
                    project_id,
                    name,
                    description,
                    auto_load.as_deref(),
                    status,
                    metadata.as_ref(),
                )
                .await
                .map_err(json_err)?
            {
                Some(_) => {
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
            let status = args.get("status").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
            let projects = engine
                .project_list(seat_id, status, limit)
                .await
                .map_err(json_err)?;
            json!({"success": true, "projects": projects.iter().map(|p| json!({
                "project_id": p.project_id, "name": p.name, "status": p.status,
                "auto_load_count": p.auto_load.len(),
            })).collect::<Vec<_>>(), "count": projects.len()})
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
            let tokens = args
                .get("page_token_limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(16000) as usize;
            engine.set_page_limit(tokens).await.map_err(json_err)?
        }
        "get_page_settings" => engine.page_settings().await.map_err(json_err)?,
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
        _ => return Err(json!({"code": -32601, "message": format!("unknown tool: {name}")})),
    };
    let _ = engine.seats.record_tool_use(seat_id, name, 0).await;
    let mut text = text.to_string();
    // Inject pending notifications into the tool response (UX channel).
    if name != "pop_notifications" {
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
    if text.chars().count() <= 2000 {
        if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            result["structuredContent"] = parsed;
        } else {
            result["structuredContent"] = json!({"text": text});
        }
    }
    // Pagination envelope: cache oversized list responses and tag them.
    if text.chars().count() > 20000 {
        let response_id = uid("resp");
        if let Ok(paginated) = engine.paginate(seat_id, &response_id, &result).await {
            if let Some(page) = paginated.get("_pagination") {
                result["_pagination"] = page.clone();
            }
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
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
}
