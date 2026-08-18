//! Раздача статики SPA (web-ui/dist) с fallback на index.html.

use crate::server::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode, header, Uri},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    uri: Uri,
) -> Response {
    let raw = uri.path().trim_start_matches('/');
    let rel = if raw.is_empty() { "index.html" } else { raw };
    // Защита от path traversal.
    let rel = rel.replace("..", "");
    let file = state.dist.join(&rel);
    let body = if file.is_file() {
        tokio::fs::read(&file).await.ok()
    } else {
        None
    };
    match body {
        Some(bytes) => {
            let mime = mime_for(&rel);
            let mut resp = Response::new(Body::from(bytes));
            if let Ok(v) = mime.parse() {
                resp.headers_mut().insert(header::CONTENT_TYPE, v);
            }
            resp
        }
        None => {
            // SPA fallback: неизвестный маршрут → index.html.
            match tokio::fs::read(state.dist.join("index.html")).await {
                Ok(bytes) => {
                    let mut resp = Response::new(Body::from(bytes));
                    if let Ok(v) = "text/html; charset=utf-8".parse() {
                        resp.headers_mut().insert(header::CONTENT_TYPE, v);
                    }
                    resp
                }
                Err(_) => (StatusCode::NOT_FOUND, "dist not built (web-ui/dist)").into_response(),
            }
        }
    }
}

// Request не используется, но сигнатура нужна для fallback-роута.
#[allow(dead_code)]
fn _assert(_: Request<Body>) {}
