//! slc-mcp — standalone binary over the SLC memory engine.
//!
//! Two entry points:
//! - `slc-mcp serve` — MCP server (JSON-RPC 2.0 over HTTP `POST /mcp`,
//!   `GET /health`, seat auth via `X-Seat-ID`).
//! - CLI subcommands for the memory pipeline (remember/compress/consolidate/
//!   search/seat/import/graveyard).

mod server;

use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use slc_core::{DocumentCategory, SlcConfig, SlcEngine, StorageKind};

#[derive(Parser)]
#[command(name = "slc-mcp", version, about = "SLC memory engine — MCP server + CLI")]
struct Cli {
    /// Obsidian vault path (default: $SLC_VAULT_PATH or ~/.slc/vault).
    #[arg(long, env = "SLC_VAULT_PATH")]
    vault: Option<String>,
    /// Use embedded SQLite instead of the Obsidian vault.
    #[arg(long)]
    sqlite: bool,
    /// Use MongoDB (SLC_MONGODB_URI, default mongodb://localhost:27017/slc).
    #[arg(long)]
    mongodb: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the MCP server (JSON-RPC over HTTP).
    Serve {
        #[arg(long, default_value_t = 3000)]
        port: u16,
        /// Auto git-commit vault changes.
        #[arg(long)]
        auto_commit: bool,
    },
    /// Health/status of the store.
    Status,
    /// Seat management.
    Seat {
        #[command(subcommand)]
        action: SeatAction,
    },
    /// Record an episodic L1 event (the diary entry).
    Remember {
        seat_id: String,
        event_id: String,
        content: String,
    },
    /// Run progressive summarization L1→L4 for a seat.
    Compress { seat_id: String },
    /// Extract learned facts (consolidation) for a seat.
    Consolidate { seat_id: String },
    /// Hybrid search over the knowledge base.
    Search {
        query: String,
        #[arg(long)]
        seat: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Import a markdown/JSON file into the KB.
    Import {
        path: String,
        #[arg(long, default_value = "custom")]
        category: String,
        #[arg(long)]
        seat: Option<String>,
    },
    /// Graveyard (soft-deleted docs).
    Graveyard {
        #[command(subcommand)]
        action: GraveyardAction,
    },
    /// Migrate a legacy Python SLC vault into the current storage
    /// (id → human-readable names, folders by category in Obsidian).
    Migrate {
        /// Legacy vault root (vault/<collection>/<stem>.md layout).
        #[arg(long)]
        from: PathBuf,
        /// Target Obsidian vault (default: $SLC_VAULT_PATH).
        #[arg(long)]
        to_vault: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SeatAction {
    List,
    Create {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    Close { seat_id: String },
}

#[derive(Subcommand)]
enum GraveyardAction {
    List,
    Cleanup {
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,slc=debug".into()),
        )
        .init();

    let cli = Cli::parse();
    // Local settings live in .env (cwd, e.g. slc-mcp/.env or repo root).
    dotenvy::dotenv().ok();
    let mut config = SlcConfig::default();
    config.storage = if cli.mongodb {
        StorageKind::MongoDB
    } else if cli.sqlite {
        StorageKind::Sqlite
    } else {
        StorageKind::ObsidianVault
    };
    if let Some(v) = cli.vault {
        config.path = v;
    }

    match cli.cmd {
        Cmd::Serve { port, auto_commit } => {
            config.auto_git_commit = auto_commit;
            if config.mcp_sampling {
                // Inference through the MCP client (sampling) — no local
                // GPU/LLM needed; embeddings degrade to text-only search.
                let (out_tx, out_rx) = tokio::sync::mpsc::channel(64);
                let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
                let llm: std::sync::Arc<dyn slc_core::LlmClient> = std::sync::Arc::new(
                    slc_core::McpSamplingLlm::new(out_tx, pending.clone()),
                );
                let engine = SlcEngine::open_async_with_llm(config, llm).await?;
                server::run(engine, port, Some(out_rx), pending).await
            } else {
                let engine = SlcEngine::open_async(config).await?;
                let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
                server::run(engine, port, None, pending).await
            }
        }
        Cmd::Migrate { from, to_vault } => {
            let engine = SlcEngine::open_async(config).await?;
            let report = slc_core::migrate::migrate_legacy_vault(&from, engine.store()).await?;
            println!("migration complete: {report:?}");
            if let Some(v) = to_vault {
                println!("target vault: {}", v.display());
            }
            Ok(())
        }
        Cmd::Status => {
            let backend = if cli.mongodb {
                "mongodb"
            } else if cli.sqlite {
                "sqlite"
            } else {
                "obsidian-vault"
            };
            println!("backend: {backend}");
            println!("path: {}", config.path);
            let engine = SlcEngine::open_async(config).await?;
            println!("health: {}", engine.health().await);
            let seats = engine.seats.list_active(100).await?;
            println!("active seats: {}", seats.len());
            Ok(())
        }
        Cmd::Seat { action } => {
            let engine = SlcEngine::open_async(config).await?;
            match action {
                SeatAction::List => {
                    for s in engine.seats.list_active(100).await? {
                        println!("{}\t{}\t{}", s.seat_id, s.name, s.last_accessed);
                    }
                    Ok(())
                }
                SeatAction::Create { id, name } => {
                    let seat = engine.seats.create_seat(id, name, None).await?;
                    println!("{}", seat.seat_id);
                    Ok(())
                }
                SeatAction::Close { seat_id } => {
                    engine.close_seat(&seat_id).await?;
                    println!("closed {seat_id}");
                    Ok(())
                }
            }
        }
        Cmd::Remember { seat_id, event_id, content } => {
            let engine = SlcEngine::open_async(config).await?;
            engine.ensure_seat(&seat_id).await?;
            engine.remember(&seat_id, &event_id, &content).await?;
            println!("recorded {event_id} for {seat_id}");
            Ok(())
        }
        Cmd::Compress { seat_id } => {
            let engine = SlcEngine::open_async(config).await?;
            let r = engine.compress(&seat_id).await?;
            println!("L1→L2: {}, L2→L3: {}, L3→L4: {}", r.l1_to_l2, r.l2_to_l3, r.l3_to_l4);
            Ok(())
        }
        Cmd::Consolidate { seat_id } => {
            let engine = SlcEngine::open_async(config).await?;
            let r = engine.consolidate(&seat_id).await?;
            println!("sources: {}, facts added: {} (total extracted: {})", r.sources, r.facts_added, r.facts_total);
            Ok(())
        }
        Cmd::Search { query, seat, limit } => {
            let engine = SlcEngine::open_async(config).await?;
            let hits = engine.search(&query, seat.as_deref(), limit).await?;
            for h in hits {
                println!(
                    "[{:.3}] {}\t({})",
                    h.rank_score,
                    h.document.document_id,
                    h.document.category.as_str()
                );
                let snippet: String = h.document.content.chars().take(120).collect();
                println!("    {snippet}");
            }
            Ok(())
        }
        Cmd::Import { path, category, seat } => {
            let engine = SlcEngine::open_async(config).await?;
            let cat = DocumentCategory::parse(&category)
                .context("bad category (core|module|task|project|code_snippet|documentation|custom|system)")?;
            let content = std::fs::read_to_string(&path)?;
            let name = std::path::Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "doc".into());
            let doc = slc_core::Document::new(
                name,
                cat,
                content,
                Default::default(),
                vec![],
                seat,
            );
            engine.add_document(&doc).await?;
            println!("imported {}", doc.document_id);
            Ok(())
        }
        Cmd::Graveyard { action } => {
            let engine = SlcEngine::open_async(config).await?;
            match action {
                GraveyardAction::List => {
                    for d in engine.store().kb_graveyard(None).await? {
                        println!("{}\t{:?}", d.document_id, d.deleted_at);
                    }
                    Ok(())
                }
                GraveyardAction::Cleanup { days } => {
                    let n = engine.store().kb_cleanup_graveyard(days).await?;
                    println!("purged {n} docs older than {days} days");
                    Ok(())
                }
            }
        }
    }
}
