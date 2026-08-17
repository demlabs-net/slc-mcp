//! slc-webui — веб-морда SLC: REST-прокси поверх MCP-сервера + раздача
//! статики SPA. Vault остаётся за одним процессом (MCP-сервер) — webui
//! общается с ним как обычный MCP-клиент (JSON-RPC по HTTP, X-Seat-ID).

mod proxy;
mod static_files;

use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, put},
    Router,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    /// MCP-сервер (http://127.0.0.1:3000 по умолчанию).
    pub mcp_url: String,
    pub http: reqwest::Client,
    /// Каталог со статикой SPA (web-ui/dist).
    pub dist: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,slc_webui=debug".into()),
        )
        .init();

    let port: u16 = std::env::var("SLC_WEBUI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3002);
    let mcp_url = std::env::var("SLC_MCP_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let dist = std::env::var("SLC_WEBUI_DIST")
        .unwrap_or_else(|_| "./web-ui/dist".into());

    let state = Arc::new(AppState {
        mcp_url,
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?,
        dist: std::path::PathBuf::from(dist),
    });

    let app = Router::new()
        // health + stats
        .route("/api/health", get(proxy::health))
        .route("/api/stats", get(proxy::stats))
        .route("/api/context", get(proxy::context))
        // documents
        .route(
            "/api/documents",
            get(proxy::list_documents).post(proxy::add_document),
        )
        .route(
            "/api/documents/{id}",
            get(proxy::get_document)
                .put(proxy::update_document)
                .delete(proxy::delete_document),
        )
        .route("/api/search", get(proxy::search))
        // tasks / projects
        .route(
            "/api/tasks",
            get(proxy::list_tasks).post(proxy::create_task),
        )
        .route(
            "/api/tasks/{id}",
            put(proxy::update_task).delete(proxy::delete_task),
        )
        .route(
            "/api/projects",
            get(proxy::list_projects).post(proxy::create_project),
        )
        .route(
            "/api/projects/{id}",
            put(proxy::update_project).delete(proxy::delete_project),
        )
        // seats, notifications, reminders, focuses
        .route("/api/seats", get(proxy::list_seats))
        .route(
            "/api/notifications",
            get(proxy::notification_list).post(proxy::pop_notifications),
        )
        .route(
            "/api/reminders",
            get(proxy::reminder_list).post(proxy::reminder_create),
        )
        .route("/api/reminders/{id}", delete(proxy::reminder_cancel))
        .route(
            "/api/focuses",
            get(proxy::focus_list).post(proxy::focus_add),
        )
        .route(
            "/api/focuses/{id}",
            put(proxy::focus_update).delete(proxy::focus_remove),
        )
        .route("/api/page", get(proxy::get_page))
        .route("/api/events", get(proxy::sse_events))
        .fallback(static_files::handler)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("SLC webui listening on http://{addr} (MCP: {})", {
        let s = std::env::var("SLC_MCP_URL").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
        s
    });
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Wait for Ctrl-C or SIGTERM (docker stop).
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
