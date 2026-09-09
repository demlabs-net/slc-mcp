//! slc-mcp — standalone binary over the SLC memory engine.
//!
//! Two entry points:
//! - `slc-mcp serve` — MCP server (JSON-RPC 2.0 over HTTP `POST /mcp`,
//!   `GET /health`, seat auth via `X-Seat-ID`).
//! - CLI subcommands for the memory pipeline (remember/compress/consolidate/
//!   search/seat/import/graveyard).

mod auth;
mod server;
mod webui;

use anyhow::Context;
use clap::{Parser, Subcommand};
use slc_core::{DocumentCategory, SlcConfig, SlcEngine, StorageKind};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "slc-mcp",
    version,
    about = "SLC memory engine — MCP server + CLI"
)]
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
    /// Migrate legacy Python SLC data into the current storage.
    Migrate {
        /// Legacy vault root (vault/<collection>/<stem>.md layout).
        #[arg(long, conflicts_with = "from_mongo")]
        from: Option<PathBuf>,
        /// Legacy MongoDB URI (e.g. mongodb://127.0.0.1:27017).
        #[arg(long, value_name = "URI", conflicts_with = "from")]
        from_mongo: Option<String>,
        /// Legacy MongoDB database (default: slc_mcp).
        #[arg(long, default_value = "slc_mcp")]
        db: String,
        /// Переименовать id документов БЗ в осмысленные с помощью standalone
        /// reasoning-провайдера из .env (`SLC_LLM` и его provider settings) и
        /// переписать auto_load/references под новые id. MCP sampling здесь
        /// недоступен, так как CLI не имеет подключённого MCP-клиента.
        /// History-доки (дневник) получают
        /// детерминированные id (дата + хвост старого id).
        #[arg(long)]
        rename_with_ai: bool,
        /// Target Obsidian vault (default: $SLC_VAULT_PATH).
        #[arg(long)]
        to_vault: Option<PathBuf>,
    },
    /// Консольный визард развертывания: выбрать провайдера эмбеддингов,
    /// при необходимости скачать модель, записать .env. Без флагов —
    /// интерактивно.
    Init {
        /// Провайдер: candle | ollama | lmstudio | hash (без вопросов).
        #[arg(long)]
        llm: Option<String>,
        /// Устройство для candle: auto | cuda | metal | cpu.
        #[arg(long)]
        device: Option<String>,
        /// Модель для candle (HF repo id, напр. BAAI/bge-m3).
        #[arg(long)]
        model: Option<String>,
    },
    /// Пересобрать эмбеддинги всех документов (или одного сита) текущим
    /// провайдером — после смены модели в настройках.
    ReindexEmbeddings {
        #[arg(long)]
        seat: Option<String>,
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
    Close {
        seat_id: String,
    },
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
            // Флаг --auto-commit дополняет env OBSIDIAN_AUTO_GIT_COMMIT,
            // а не перезаписывает его (иначе авто-коммит vault выключен
            // всегда, когда флаг не передан).
            config.auto_git_commit = auto_commit || config.auto_git_commit;
            let dist = std::env::var("SLC_WEBUI_DIST").unwrap_or_else(|_| "./web-ui/dist".into());
            let auth_state = auth::AuthState::from_env(&config.path);
            auth_state.store.load().context("auth store load")?;
            if config.mcp_sampling {
                // Inference through the MCP client (sampling) — no local
                // GPU/LLM needed; embeddings degrade to text-only search.
                let (out_tx, out_rx) = tokio::sync::mpsc::channel(64);
                let pending =
                    std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
                let llm: std::sync::Arc<dyn slc_core::LlmClient> =
                    std::sync::Arc::new(slc_core::McpSamplingLlm::new(out_tx, pending.clone()));
                let engine = SlcEngine::open_async_with_llm(config, llm).await?;
                server::run(engine, port, dist.into(), auth_state, Some(out_rx), pending).await
            } else {
                let engine = SlcEngine::open_async(config).await?;
                let pending =
                    std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
                server::run(engine, port, dist.into(), auth_state, None, pending).await
            }
        }
        Cmd::Migrate {
            from,
            from_mongo,
            db,
            rename_with_ai,
            to_vault,
        } => {
            if let Some(v) = to_vault.clone() {
                config.path = v.to_string_lossy().to_string();
            }
            let engine = SlcEngine::open_async(config).await?;
            if let Some(uri) = from_mongo {
                let opts = slc_core::migrate::MongoMigrateOptions { uri, database: db };
                let llm = if rename_with_ai {
                    Some(engine.llm())
                } else {
                    None
                };
                let report = slc_core::migrate::migrate_legacy_mongo(
                    &opts,
                    engine.store(),
                    llm,
                    rename_with_ai,
                )
                .await?;
                println!("mongo migration complete: {report:?}");
            } else if let Some(from) = from {
                let report = slc_core::migrate::migrate_legacy_vault(&from, engine.store()).await?;
                println!("migration complete: {report:?}");
            } else {
                anyhow::bail!("укажи источник: --from <legacy-vault> или --from-mongo [URI]");
            }
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
        Cmd::Remember {
            seat_id,
            event_id,
            content,
        } => {
            let engine = SlcEngine::open_async(config).await?;
            engine.ensure_seat(&seat_id).await?;
            engine.remember(&seat_id, &event_id, &content).await?;
            println!("recorded {event_id} for {seat_id}");
            Ok(())
        }
        Cmd::Compress { seat_id } => {
            let engine = SlcEngine::open_async(config).await?;
            let r = engine.compress(&seat_id).await?;
            println!(
                "L1→L2: {}, L2→L3: {}, L3→L4: {}",
                r.l1_to_l2, r.l2_to_l3, r.l3_to_l4
            );
            Ok(())
        }
        Cmd::Consolidate { seat_id } => {
            let engine = SlcEngine::open_async(config).await?;
            let r = engine.consolidate(&seat_id).await?;
            println!(
                "sources: {}, facts added: {} (total extracted: {})",
                r.sources, r.facts_added, r.facts_total
            );
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
        Cmd::Import {
            path,
            category,
            seat,
        } => {
            let engine = SlcEngine::open_async(config).await?;
            let cat = DocumentCategory::parse(&category).context(
                "bad category (core|module|task|project|code_snippet|documentation|custom|system)",
            )?;
            let content = std::fs::read_to_string(&path)?;
            let name = std::path::Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "doc".into());
            let mut doc =
                slc_core::Document::new(name, cat, content, Default::default(), vec![], seat);
            engine.add_document(&mut doc).await?;
            println!(
                "imported {} → {}",
                doc.document_id,
                doc.folder.clone().unwrap_or_else(|| doc.default_folder())
            );
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
        Cmd::Init { llm, device, model } => cmd_init(config, llm, device, model).await,
        Cmd::ReindexEmbeddings { seat } => {
            let engine = SlcEngine::open_async(config).await?;
            let report = engine.reindex_embeddings(seat.as_deref()).await?;
            println!("reindex complete: {report:?}");
            // Exit immediately: tokio's graceful shutdown waits for
            // background spawn_blocking tasks (git commit on a large
            // vault) and the process hangs in futex_wait forever after
            // the work is done. Flush stdout first — exit() skips the
            // normal flush of buffered stdio (the report was observed
            // missing from redirected logs).
            use std::io::Write;
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        }
    }
}

// ── `slc-mcp init` — консольный визард развертывания ───────────────

/// Прочитать строку ответа (пустая строка → default).
fn ask(prompt: &str, default: &str) -> String {
    print!("{prompt}");
    if !default.is_empty() {
        print!(" [{}]", default);
    }
    print!(": ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    let answer = line.trim().to_string();
    if answer.is_empty() {
        default.to_string()
    } else {
        answer
    }
}

/// Записать/обновить ключ в .env (cwd). Существующие строки заменяются.
fn set_env_line(key: &str, value: &str) -> std::io::Result<PathBuf> {
    let path = std::env::current_dir()?.join(".env");
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default();
    let entry = format!("{key}={value}");
    match lines.iter().position(|l| l.starts_with(&format!("{key}="))) {
        Some(i) => lines[i] = entry.clone(),
        None => lines.push(entry.clone()),
    }
    std::fs::write(&path, lines.join("\n") + "\n")?;
    Ok(path)
}

async fn cmd_init(
    config: SlcConfig,
    llm_arg: Option<String>,
    device_arg: Option<String>,
    model_arg: Option<String>,
) -> anyhow::Result<()> {
    use slc_core::LlmClient as _;

    println!("── SLC setup ────────────────────────────────────────────");

    // 1. Провайдер.
    let llm = match llm_arg {
        Some(v) => v,
        None => ask(
            "LLM-провайдер (1: candle, 2: ollama, 3: lmstudio, 4: hash)",
            "candle",
        ),
    };
    let llm = match llm.as_str() {
        "1" | "candle" => "candle".to_string(),
        "2" | "ollama" => "ollama".to_string(),
        "3" | "lmstudio" => "lmstudio".to_string(),
        "4" | "hash" => "hash".to_string(),
        other => {
            anyhow::bail!("unknown provider: {other} (candle|ollama|lmstudio|hash)")
        }
    };

    let mut device = device_arg;
    let mut model = model_arg;

    if llm == "candle" {
        // 2. Устройство: показываем автодетект.
        let detected = slc_core::candle_emb::detect_device_name();
        println!("определено устройство: {detected}");
        if device.is_none() {
            device = Some(ask("устройство (auto|cuda|metal|cpu)", "auto"));
        }
        // 3. Модель: дефолт по устройству.
        let dev = device.as_deref().unwrap_or("auto");
        let default_model = slc_core::candle_emb::CandleEmbeddingLlm::default_model_for(dev);
        if model.is_none() {
            model = Some(ask("модель (HF repo id)", &default_model));
        }
        let repo_id = model.as_deref().unwrap_or(&default_model).to_string();

        // 4. Скачивание (единственное место, где качается модель).
        if !slc_core::candle_emb::model_is_cached(&repo_id) {
            println!("скачиваю модель {repo_id} в кэш Hugging Face…");
            slc_core::candle_emb::download_embedding_model(&repo_id).map_err(anyhow::Error::msg)?;
        } else {
            println!("модель {repo_id} уже в кэше");
        }

        // 5. Проверка: загрузить и сделать контрольный эмбеддинг ДО записи
        //    .env — при провале (например, metal без layer-norm) конфигурация
        //    не должна остаться в битом состоянии.
        let llm: std::sync::Arc<dyn slc_core::LlmClient> = std::sync::Arc::new(
            slc_core::candle_emb::CandleEmbeddingLlm::with_config(&repo_id, dev),
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(120),
            llm.generate_embedding("проверка эмбеддинга"),
        )
        .await
        {
            Ok(Ok(v)) => println!("✅ модель готова: dim={} (device={dev})", v.len()),
            Ok(Err(e)) => anyhow::bail!(
                "модель не загрузилась: {e} — .env не записан, попробуй другое устройство"
            ),
            Err(_) => anyhow::bail!("таймаут загрузки модели — .env не записан"),
        }

        // 6. Запись в .env (после успешной проверки).
        let env_path = set_env_line("SLC_LLM", "candle")?;
        set_env_line("SLC_EMBED_DEVICE", dev)?;
        set_env_line("SLC_EMBED_MODEL", &repo_id)?;
        println!("конфигурация записана в {}", env_path.display());
        println!("дальше: slc-mcp serve (эмбеддинги уже в кэше, автоскачивание не требуется)");
    } else {
        // Внешний провайдер / hash: только .env.
        let env_path = set_env_line("SLC_LLM", &llm)?;
        println!("конфигурация записана в {}", env_path.display());
        match llm.as_str() {
            "ollama" => println!(
                "убедись, что Ollama запущен (OLLAMA_ENDPOINT, default http://localhost:11434)"
            ),
            "lmstudio" => println!("укажи LMSTUDIO_URL (OpenAI-совместимый сервер) в .env"),
            _ => println!("CPU-hash: эмбеддинги без моделей; семантика ограниченная"),
        }
    }
    let _ = config;
    Ok(())
}
