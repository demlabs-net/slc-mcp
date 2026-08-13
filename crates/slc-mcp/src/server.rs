//! Minimal MCP server: JSON-RPC 2.0 over HTTP (`POST /mcp`), streamable-HTTP
//! style without SSE for now. Tools are the canonical snake_case catalog
//! (search, get_document, add_document, remember, context_load, seat_info).
//! Seat auth: `X-Seat-ID` header (legacy_seat_id mode).

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{json, Value};
use slc_core::{DocumentCategory, DocMeta, Document, SlcEngine};
use std::sync::Arc;

pub struct AppState {
    pub engine: Arc<SlcEngine>,
}

pub async fn run(engine: SlcEngine, port: u16) -> anyhow::Result<()> {
    if let Err(e) = engine.start_background().await {
        tracing::warn!("failed to start background timers: {e}");
    }
    let state = Arc::new(AppState { engine: Arc::new(engine) });
    let app = Router::new()
        .route("/mcp", post(mcp))
        .route("/health", get(health))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("SLC MCP listening on http://{addr}/mcp");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "status": if state.engine.health().await { "ok" } else { "degraded" },
        "server": concat!("slc-mcp ", env!("CARGO_PKG_VERSION")),
        "services": { "storage": state.engine.health().await },
    }))
}

/// Resolve the seat from the `X-Seat-ID` header (auto-provision on miss).
fn seat_from_request(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("x-seat-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn mcp(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    let engine = state.engine.as_ref();
    let seat_id = seat_from_request(&headers);
    // Tool calls need a seat (legacy_seat_id mode); initialize/list don't.
    if !matches!(method, "initialize" | "tools/list" | "ping") && seat_id.is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"missing X-Seat-ID header"}})),
        );
    }

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-03-26",
            "capabilities": { "tools": {}, "prompts": {} },
            "serverInfo": { "name": "slc-mcp", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(Value::Null),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "prompts/list" => Ok(json!({ "prompts": prompts() })),
        "prompts/get" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name == "check_notifications" {
                Ok(json!({
                    "name": "check_notifications",
                    "description": "Pop pending notifications for this seat and report them to the user",
                    "arguments": [{ "name": "limit", "description": "max notifications (default 5)", "required": false }],
                }))
            } else {
                Err(json!({"code": -32602, "message": format!("unknown prompt: {name}")}))
            }
        }
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let seat = seat_id.clone().unwrap_or_default();
            call_tool(engine, &seat, name, &args).await
        }
        _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };

    match result {
        Ok(result) => (StatusCode::OK, Json(json!({"jsonrpc":"2.0","id":id,"result":result}))),
        Err(error) => (StatusCode::OK, Json(json!({"jsonrpc":"2.0","id":id,"error":error}))),
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
            "category": {"type":"string","enum":["core","module","task","project","code_snippet","documentation","custom","system"]},
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
        "name": "idea_add",
        "description": "Add an idea to the proactive idea pool",
        "inputSchema": {"type":"object","properties":{
            "content": {"type":"string"},
            "source": {"type":"string","default":"manual"},
            "mind_type": {"type":"string","enum":["front","planner","executor","critic","shared"]}
        },"required":["content"]}
    }),
    json!({
        "name": "idea_list",
        "description": "List active ideas for the seat",
        "inputSchema": {"type":"object","properties":{
            "limit": {"type":"number","default":20},
            "mind_type": {"type":"string","enum":["front","planner","executor","critic","shared"]}
        },"required":[]}
    }),
    json!({
        "name": "idea_random",
        "description": "Surface a random idea, weighted toward fresh rarely-shown ones",
        "inputSchema": {"type":"object","properties":{
            "mind_type": {"type":"string","enum":["front","planner","executor","critic","shared"]}
        },"required":[]}
    }),
    json!({
        "name": "idea_remove",
        "description": "Remove an idea from the pool",
        "inputSchema": {"type":"object","properties":{
            "idea_id": {"type":"string"}
        },"required":["idea_id"]}
    }),
    json!({
        "name": "reflect_now",
        "description": "Run one reflection pass: recent history + focuses → new ideas",
        "inputSchema": {"type":"object","properties":{},"required":[]}
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
]
}

fn prompts() -> Vec<Value> {
    vec![
    json!({
        "name": "check_notifications",
        "description": "Pop pending notifications for this seat and report them to the user",
    }),
    ]
}

async fn call_tool(engine: &SlcEngine, seat_id: &str, name: &str, args: &Value) -> Result<Value, Value> {
    engine.seats.ensure_seat(seat_id).await.map_err(|e| json!({"code": -32000, "message": e.to_string()}))?;
    let text = match name {
        "search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let hits = engine.search(query, Some(seat_id), limit).await.map_err(json_err)?;
            json!({"results": hits.iter().map(|h| json!({
                "document_id": h.document.document_id,
                "category": h.document.category.as_str(),
                "folder": h.document.folder,
                "score": h.rank_score,
                "content": truncate(&h.document.content, 2000),
            })).collect::<Vec<_>>()})
        }
        "get_document" => {
            let id = args.get("document_id").and_then(|v| v.as_str()).unwrap_or("");
            match engine.get_document(id).await.map_err(json_err)? {
                Some(d) => json!({"document_id": d.document_id, "category": d.category.as_str(), "content": d.content, "tags": d.tags, "metadata": d.metadata}),
                None => json!({"error": format!("not found: {id}")}),
            }
        }
        "add_document" => {
            let id = args.get("document_id").and_then(|v| v.as_str()).unwrap_or("");
            let category = DocumentCategory::parse(args.get("category").and_then(|v| v.as_str()).unwrap_or("custom"))
                .ok_or_else(|| json!({"code": -32602, "message": "bad category"}))?;
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let folder = args.get("folder").and_then(|v| v.as_str()).map(String::from);
            let doc = Document::with_folder(id, category, folder, content, DocMeta::default(), vec![], Some(seat_id.into()));
            engine.add_document(&doc).await.map_err(json_err)?;
            json!({"document_id": doc.document_id})
        }
        "remember" => {
            let event_id = args.get("event_id").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            engine.remember(seat_id, event_id, content).await.map_err(json_err)?;
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
        "seat_info" => {
            match engine.seats.get_seat(seat_id).await.map_err(json_err)? {
                Some(s) => json!({"seat_id": s.seat_id, "status": format!("{:?}", s.status), "usage_stats": s.usage_stats}),
                None => json!({"seat_id": seat_id}),
            }
        }
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
            let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let priority = args.get("priority").and_then(|v| v.as_i64()).unwrap_or(5);
            let depends_on: Vec<String> = args.get("depends_on").and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let mind_type = args.get("mind_type").and_then(|v| v.as_str()).map(String::from);
            let item = engine.focus_add(seat_id, title, description, priority, &depends_on, mind_type.as_deref()).await.map_err(json_err)?;
            json!({"focus_id": item.focus_id, "priority": item.priority})
        }
        "focus_list" => {
            let mind_type = args.get("mind_type").and_then(|v| v.as_str()).and_then(parse_mind);
            let items = engine.focus_list(seat_id, mind_type).await.map_err(json_err)?;
            json!({"focuses": items.iter().map(|f| json!({
                "focus_id": f.focus_id, "title": f.title, "description": f.description,
                "priority": f.priority, "depends_on": f.depends_on,
            })).collect::<Vec<_>>()})
        }
        "focus_update" => {
            let focus_id = args.get("focus_id").and_then(|v| v.as_str()).unwrap_or("");
            let title = args.get("title").and_then(|v| v.as_str()).map(String::from);
            let description = args.get("description").and_then(|v| v.as_str()).map(String::from);
            let priority = args.get("priority").and_then(|v| v.as_i64());
            let depends_on: Option<Vec<String>> = args.get("depends_on").and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
            let ok = engine.focus_update(seat_id, focus_id, title.as_deref(), description.as_deref(), priority, depends_on.as_deref()).await.map_err(json_err)?;
            json!({"updated": ok})
        }
        "focus_remove" => {
            let focus_id = args.get("focus_id").and_then(|v| v.as_str()).unwrap_or("");
            let ok = engine.focus_remove(seat_id, focus_id).await.map_err(json_err)?;
            json!({"removed": ok})
        }
        "idea_add" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("manual");
            let mind_type = args.get("mind_type").and_then(|v| v.as_str()).map(String::from);
            let item = engine.idea_add(seat_id, content, source, None, mind_type.as_deref()).await.map_err(json_err)?;
            json!({"idea_id": item.idea_id, "source": item.source})
        }
        "idea_list" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let mind_type = args.get("mind_type").and_then(|v| v.as_str()).and_then(parse_mind);
            let items = engine.idea_list(seat_id, limit, mind_type).await.map_err(json_err)?;
            json!({"ideas": items.iter().map(|i| json!({
                "idea_id": i.idea_id, "content": i.content, "source": i.source,
            })).collect::<Vec<_>>()})
        }
        "idea_random" => {
            let mind_type = args.get("mind_type").and_then(|v| v.as_str()).and_then(parse_mind);
            match engine.idea_random(seat_id, mind_type).await.map_err(json_err)? {
                Some(i) => json!({"idea_id": i.idea_id, "content": i.content, "source": i.source}),
                None => json!({"idea_id": null, "message": "no active ideas"}),
            }
        }
        "idea_remove" => {
            let idea_id = args.get("idea_id").and_then(|v| v.as_str()).unwrap_or("");
            let ok = engine.idea_remove(seat_id, idea_id).await.map_err(json_err)?;
            json!({"removed": ok})
        }
        "reflect_now" => {
            engine.reflect(seat_id).await.map_err(json_err)?;
            json!({"reflected": true})
        }
        "reminder_create" => {
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let remind_at = args.get("remind_at").and_then(|v| v.as_str()).unwrap_or("");
            let mind_type = args.get("mind_type").and_then(|v| v.as_str());
            let dt = slc_core::parse_remind_at(remind_at).map_err(json_err)?;
            let r = engine.reminder_create(seat_id, content, dt, mind_type).await.map_err(json_err)?;
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
            let reminder_id = args.get("reminder_id").and_then(|v| v.as_str()).unwrap_or("");
            let ok = engine.reminder_cancel(seat_id, reminder_id).await.map_err(json_err)?;
            json!({"cancelled": ok})
        }
        "pop_notifications" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;
            let items = engine.pop_notifications(seat_id, limit).await.map_err(json_err)?;
            json!({"notifications": items.iter().map(|n| json!({
                "notification_id": n.notification_id, "source": n.source,
                "title": n.title, "body": n.body, "metadata": n.metadata,
            })).collect::<Vec<_>>()})
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
    Ok(json!({"content": [{"type": "text", "text": text}], "isError": false}))
}

fn json_err(e: slc_core::SlcError) -> Value {
    json!({"code": -32000, "message": e.to_string()})
}

fn parse_mind(s: &str) -> Option<slc_core::MindType> {
    slc_core::MindType::parse(s).ok()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
