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
fn seat_from_request(headers: &axum::http::HeaderMap, engine: &SlcEngine) -> Option<String> {
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
    let seat_id = seat_from_request(&headers, engine);
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
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "slc-mcp", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(Value::Null),
        "tools/list" => Ok(json!({ "tools": tools() })),
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
        _ => return Err(json!({"code": -32601, "message": format!("unknown tool: {name}")})),
    };
    let _ = engine.seats.record_tool_use(seat_id, name, 0).await;
    Ok(json!({"content": [{"type": "text", "text": text.to_string()}], "isError": false}))
}

fn json_err(e: slc_core::SlcError) -> Value {
    json!({"code": -32000, "message": e.to_string()})
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}
