//! slc-webui — веб-морда SLC: REST-сервис поверх встроенного slc-core
//! движка (не прокси над MCP) + раздача статики SPA.
//!
//! Vault открывается этим же процессом (Obsidian, тот же путь, что у
//! MCP-сервера). Multi-process синхронизация: периодический `refresh`
//! (SLC_VAULT_REFRESH_SECS, default 30с) — файлы vault являются источником
//! истины. Авто-коммит/push vault — через OBSIDIAN_AUTO_GIT_COMMIT.

mod api;
mod static_files;

use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
    Router,
};
use slc_core::{SlcConfig, SlcEngine, StorageKind};
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<SlcEngine>,
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
    let dist = std::env::var("SLC_WEBUI_DIST")
        .unwrap_or_else(|_| "./web-ui/dist".into());

    // Конфиг полностью из env (SLC_VAULT_PATH, SLC_LLM/provider settings,
    // SLC_CONTEXT_LIMIT_TOKENS, OBSIDIAN_AUTO_GIT_COMMIT, SLC_AI_ORGANIZE…).
    let config = SlcConfig::default();
    if config.storage != StorageKind::ObsidianVault {
        anyhow::bail!("webui supports only the Obsidian vault backend");
    }
    let engine = Arc::new(SlcEngine::open_async(config).await?);
    // Таймеры (напоминания/компрессия) + refresh-луп multi-process синка.
    engine.start_background().await?;

    let state = Arc::new(AppState {
        engine,
        dist: std::path::PathBuf::from(dist),
    });

    let app = Router::new()
        .route("/api/health", get(api::health))
        .route("/api/stats", get(api::stats))
        .route("/api/context", get(api::context))
        .route(
            "/api/documents",
            get(api::list_documents).post(api::add_document),
        )
        .route(
            "/api/documents/{id}",
            get(api::get_document)
                .put(api::update_document)
                .delete(api::delete_document),
        )
        .route("/api/search", get(api::search))
        .route(
            "/api/tasks",
            get(api::list_tasks).post(api::create_task),
        )
        .route(
            "/api/tasks/{id}",
            put(api::update_task).delete(api::delete_task),
        )
        .route(
            "/api/projects",
            get(api::list_projects).post(api::create_project),
        )
        .route(
            "/api/projects/{id}",
            put(api::update_project).delete(api::delete_project),
        )
        .route("/api/seats", get(api::list_seats))
        .route(
            "/api/notifications",
            get(api::notification_list).post(api::pop_notifications),
        )
        .route(
            "/api/reminders",
            get(api::reminder_list).post(api::reminder_create),
        )
        .route("/api/reminders/{id}", delete(api::reminder_cancel))
        .route(
            "/api/focuses",
            get(api::focus_list).post(api::focus_add),
        )
        .route(
            "/api/focuses/{id}",
            put(api::focus_update).delete(api::focus_remove),
        )
        .route("/api/events", get(api::sse_events))
        .fallback(static_files::handler)
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("SLC webui listening on http://{addr} (embedded slc-core)");
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
