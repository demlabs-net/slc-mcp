//! Onboard embedding inference via candle — no model server required.
//!
//! [`CandleEmbeddingLlm`] loads an embedding model that was downloaded by
//! `slc-mcp init` (cached in `~/.cache/huggingface`) and runs it locally on
//! the GPU (CUDA on Linux, Metal on macOS) or the CPU:
//!
//! - **GPU** → `BAAI/bge-m3` (multilingual, RU-first, 1024-dim);
//! - **CPU** → `intfloat/multilingual-e5-small` (384-dim).
//!
//! The model is NEVER auto-downloaded on first use: if it is not in the HF
//! cache, embedding calls fail fast with a pointer to `slc-mcp init`, and
//! search degrades to text-only. Loading from the cache happens on a
//! background thread, so server startup never blocks.
//!
//! Configuration (env, written by `slc-mcp init`):
//! - `SLC_EMBED_MODEL` — Hugging Face repo id (e.g. `BAAI/bge-m3`);
//! - `SLC_EMBED_DEVICE` — `auto` (default) | `cuda` | `metal` | `cpu`.

use crate::error::{SlcError, SlcResult};
use crate::llm::{EmbeddingKind, LlmClient};
use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use std::sync::{Arc, Mutex};

/// GPU-модель по умолчанию: мультиязычная (включая русский), 1024-мер.
pub const BGE_M3: &str = "BAAI/bge-m3";
/// CPU-модель по умолчанию: лёгкая, 384-мер, тоже мультиязычная.
pub const E5_SMALL: &str = "intfloat/multilingual-e5-small";

/// Файлы модели (safetensors-вариант; часть моделей, например bge-m3,
/// публикует только `pytorch_model.bin` — см. [`PTH_FILE`]).
const MODEL_FILES: [&str; 3] = ["model.safetensors", "config.json", "tokenizer.json"];
/// Fallback для моделей без safetensors (torch-формат, читается candle'ом).
const PTH_FILE: &str = "pytorch_model.bin";

/// Максимальная длина токенов для эмбеддинга (у e5-small лимит 512).
const MAX_TOKENS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceKind {
    Auto,
    Cuda,
    Metal,
    Cpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pooling {
    /// bge-m3: первый токен.
    Cls,
    /// e5-семейство: среднее по всем токенам.
    Mean,
}

enum EncodeModel {
    Bert(candle_transformers::models::bert::BertModel),
    XlmRoberta(candle_transformers::models::xlm_roberta::XLMRobertaModel),
}

struct Loaded {
    model: EncodeModel,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
    dim: usize,
    pooling: Pooling,
    /// e5-семейство требует префиксов query:/passage:.
    e5_prefix: bool,
    /// Id токена паддинга (для батч-инференса).
    pad_id: u32,
}

enum LoadState {
    Loading,
    Ready(Arc<Loaded>),
    Failed(String),
}

/// Onboard embedding client (candle). The model is loaded from the HF cache
/// in the background; embedding calls await it without blocking a tokio
/// worker (async readiness via `Notify`), and the actual inference runs on
/// the blocking pool (`spawn_blocking`). If the model was never downloaded
/// (`slc-mcp init`), calls fail fast instead.
pub struct CandleEmbeddingLlm {
    repo_id: String,
    device_kind: DeviceKind,
    state: Arc<Mutex<LoadState>>,
    notify: Arc<tokio::sync::Notify>,
}

impl Default for CandleEmbeddingLlm {
    fn default() -> Self {
        Self::new()
    }
}

impl CandleEmbeddingLlm {
    /// Общий конструктор: резолвит настройки из env.
    pub fn new() -> Self {
        let device = std::env::var("SLC_EMBED_DEVICE").unwrap_or_default();
        let device = match device.as_str() {
            "cuda" | "metal" | "cpu" => device,
            _ => "auto".to_string(),
        };
        let repo = std::env::var("SLC_EMBED_MODEL").ok();
        Self::with_config(
            repo.unwrap_or_else(|| Self::default_model_for(&device)),
            &device,
        )
    }

    /// Конструктор с явной конфигурацией (используется `slc-mcp init`
    /// для контрольной проверки без чтения env). `device` — `auto` |
    /// `cuda` | `metal` | `cpu`.
    pub fn with_config(repo_id: impl Into<String>, device: &str) -> Self {
        let device_kind = match device {
            "cuda" => DeviceKind::Cuda,
            "metal" => DeviceKind::Metal,
            "cpu" => DeviceKind::Cpu,
            _ => detect_device(),
        };
        let repo_id = repo_id.into();
        let llm = Self {
            repo_id,
            device_kind,
            state: Arc::new(Mutex::new(LoadState::Loading)),
            notify: Arc::new(tokio::sync::Notify::new()),
        };
        if llm.model_cached() {
            let worker = llm.clone_for_loader();
            std::thread::Builder::new()
                .name("slc-embed-loader".into())
                .spawn(move || worker.load_in_background())
                .expect("spawn slc-embed-loader");
        } else {
            *llm.state.lock().unwrap() = LoadState::Failed(format!(
                "model {} is not in the cache — run `slc-mcp init` to download it",
                llm.repo_id
            ));
        }
        llm
    }

    /// True when the caller asked for a GPU-backed model (used by the engine
    /// to decide the default: GPU → onboard candle, otherwise → CPU-hash).
    pub fn gpu_requested(&self) -> bool {
        self.device_kind != DeviceKind::Cpu && self.device_kind != DeviceKind::Auto
            || self.device_kind == DeviceKind::Auto && detect_device() != DeviceKind::Cpu
    }

    /// Статическая проверка «есть ли GPU-бэкенд» (без создания клиента и
    /// фонового лоадера) — для выбора провайдера по умолчанию.
    pub fn gpu_available() -> bool {
        detect_device() != DeviceKind::Cpu
    }

    /// True when all model files are already in the Hugging Face cache
    /// (checks the cache only — no network). Used by `slc-mcp init` and the
    /// engine's provider selection.
    pub fn model_cached(&self) -> bool {
        let (owner, name) = match self.repo_id.split_once('/') {
            Some(p) => p,
            None => return false,
        };
        let client = match hf_hub::HFClientSync::new() {
            Ok(c) => c,
            Err(_) => return false,
        };
        let repo = client.model(owner, name);
        let cached = |file: &str| {
            repo.download_file()
                .filename(file.to_string())
                .local_files_only(true)
                .send()
                .is_ok()
        };
        // Вес — safetensors ИЛИ torch-фоллбэк.
        (cached(MODEL_FILES[0]) || cached(PTH_FILE))
            && cached(MODEL_FILES[1])
            && cached(MODEL_FILES[2])
    }

    /// Default model for a device kind (what `slc-mcp init` proposes).
    /// `auto` resolves the real device first: GPU → bge-m3, CPU → e5-small.
    pub fn default_model_for(device_kind: &str) -> String {
        match device_kind {
            "cuda" | "metal" => BGE_M3.to_string(),
            "cpu" => E5_SMALL.to_string(),
            _ => match detect_device() {
                DeviceKind::Cuda | DeviceKind::Metal => BGE_M3.to_string(),
                _ => E5_SMALL.to_string(),
            },
        }
    }

    /// Копия для фонового лоадера: разделяет `state`/`notify` с оригиналом.
    fn clone_for_loader(&self) -> Self {
        Self {
            repo_id: self.repo_id.clone(),
            device_kind: self.device_kind,
            state: Arc::clone(&self.state),
            notify: Arc::clone(&self.notify),
        }
    }

    fn load_in_background(&self) {
        match self.load() {
            Ok(loaded) => {
                tracing::info!(model = %self.repo_id, dim = loaded.dim, "embedding model ready");
                *self.state.lock().unwrap() = LoadState::Ready(Arc::new(loaded));
            }
            Err(e) => {
                tracing::warn!(model = %self.repo_id, "embedding model load failed: {e} — text-only search; try SLC_LLM=hash or SLC_EMBED_MODEL");
                *self.state.lock().unwrap() = LoadState::Failed(e);
            }
        }
        self.notify.notify_waiters();
    }

    /// Async-ожидание готовности модели — НЕ блокирует tokio-поток
    /// (в отличие от старого Condvar). Будущее создаётся до проверки
    /// состояния, поэтому уведомление между проверкой и await не теряется.
    async fn wait_ready(&self) -> Result<Arc<Loaded>, SlcError> {
        loop {
            let notified = self.notify.notified();
            {
                let state = self.state.lock().unwrap();
                match &*state {
                    LoadState::Ready(l) => return Ok(l.clone()),
                    LoadState::Failed(e) => {
                        return Err(SlcError::Storage(format!(
                            "embedding model unavailable: {e}"
                        )));
                    }
                    LoadState::Loading => {}
                }
            }
            notified.await;
        }
    }

    fn resolve_device(&self) -> Result<Device, String> {
        match self.device_kind {
            DeviceKind::Cpu => Ok(Device::Cpu),
            DeviceKind::Cuda => {
                #[cfg(feature = "cuda")]
                {
                    return Device::cuda_if_available(0).map_err(|e| format!("CUDA: {e}"));
                }
                #[cfg(not(feature = "cuda"))]
                Err("built without the `cuda` feature (needs the CUDA toolkit)".into())
            }
            DeviceKind::Metal => {
                #[cfg(all(target_os = "macos", feature = "candle-emb"))]
                {
                    return Device::new_metal(0).map_err(|e| format!("Metal: {e}"));
                }
                #[cfg(not(all(target_os = "macos", feature = "candle-emb")))]
                Err("built without the Metal backend (macOS only)".into())
            }
            DeviceKind::Auto => Err("auto device must be resolved before load".into()),
        }
    }

    fn load(&self) -> Result<Loaded, String> {
        let device = match self.device_kind {
            DeviceKind::Auto => detect_and_resolve_device()?,
            _ => self.resolve_device()?,
        };
        let client = hf_hub::HFClientSync::new().map_err(|e| format!("hf-hub: {e}"))?;
        let (owner, name) = self
            .repo_id
            .split_once('/')
            .ok_or_else(|| format!("SLC_EMBED_MODEL must be owner/name, got: {}", self.repo_id))?;
        let repo = client.model(owner, name);

        let dl = |file: &str| -> Result<std::path::PathBuf, String> {
            // local_files_only: никаких автоскачиваний — модель должна быть
            // подготовлена через `slc-mcp init`.
            repo.download_file()
                .filename(file.to_string())
                .local_files_only(true)
                .send()
                .map_err(|e| format!("{file} (запусти `slc-mcp init`, чтобы скачать модель): {e}"))
        };
        // Веса: предпочитаем safetensors, иначе torch-файл (bge-m3 и др.).
        let safetensors = dl(MODEL_FILES[0]).is_ok();
        let weights = if safetensors {
            dl(MODEL_FILES[0])?
        } else {
            dl(PTH_FILE)?
        };
        let config_path = dl(MODEL_FILES[1])?;
        let tokenizer_path = dl(MODEL_FILES[2])?;

        let config_json =
            std::fs::read_to_string(config_path).map_err(|e| format!("read config: {e}"))?;
        let cfg: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|e| format!("parse config: {e}"))?;
        let dim = cfg
            .pointer("/hidden_size")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .ok_or_else(|| "config.json: missing hidden_size".to_string())?;
        let architecture = cfg
            .pointer("/architectures/0")
            .and_then(|v| v.as_str())
            .unwrap_or("BertModel")
            .to_string();

        let vb = if safetensors {
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights], DType::F32, &device) }
                .map_err(|e| format!("load safetensors: {e}"))?
        } else {
            // torch-формат читается целиком (pickle) — 4-5 ГБ в RAM для
            // bge-m3; это GPU-путь, для слабых машин есть e5-small/hash.
            VarBuilder::from_pth(&weights, DType::F32, &device)
                .map_err(|e| format!("load pytorch_model.bin: {e}"))?
        };
        let mut tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path)
            .map_err(|e| format!("load tokenizer: {e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                direction: tokenizers::TruncationDirection::Right,
                max_length: MAX_TOKENS,
                stride: 0,
                strategy: tokenizers::TruncationStrategy::LongestFirst,
            }))
            .map_err(|e| format!("tokenizer truncation: {e}"))?;

        // bge-m3 — XLM-RoBERTa (CLS pooling, без префиксов); e5 — BERT
        // (mean pooling, префиксы query:/passage:).
        let pooling = if self.repo_id.contains("bge-m3") {
            Pooling::Cls
        } else {
            Pooling::Mean
        };
        let e5_prefix = self.repo_id.contains("e5");

        let model = if architecture == "XlmRobertaModel" {
            let config: candle_transformers::models::xlm_roberta::Config =
                serde_json::from_str(&config_json).map_err(|e| format!("parse xlm config: {e}"))?;
            let m = candle_transformers::models::xlm_roberta::XLMRobertaModel::new(&config, vb)
                .map_err(|e| format!("load xlm-roberta: {e}"))?;
            EncodeModel::XlmRoberta(m)
        } else {
            let config: candle_transformers::models::bert::Config =
                serde_json::from_str(&config_json)
                    .map_err(|e| format!("parse bert config: {e}"))?;
            let m = candle_transformers::models::bert::BertModel::load(vb, &config)
                .map_err(|e| format!("load bert: {e}"))?;
            EncodeModel::Bert(m)
        };

        // Токен паддинга для батч-инференса (<pad> в bert/xlm-r словарях).
        let pad_id = tokenizer.token_to_id("<pad>").unwrap_or(0);

        Ok(Loaded {
            model,
            tokenizer,
            device,
            dim,
            pooling,
            e5_prefix,
            pad_id,
        })
    }

    /// Синхронный эмбеддинг одного текста (токенизация + forward + pooling).
    /// Вызывается только из `spawn_blocking` — candle-операции блокирующие.
    fn embed_loaded(loaded: &Loaded, text: &str, kind: EmbeddingKind) -> SlcResult<Vec<f32>> {
        let embed = (|| -> Result<Vec<f32>, String> {
            let text = match (loaded.e5_prefix, kind) {
                (true, EmbeddingKind::Query) => format!("query: {text}"),
                (true, EmbeddingKind::Passage) => format!("passage: {text}"),
                _ => text.to_string(),
            };
            let enc = loaded
                .tokenizer
                .encode(text, true)
                .map_err(|e| format!("tokenize: {e}"))?;
            let ids = enc.get_ids();
            if ids.is_empty() {
                return Ok(vec![0.0f32; loaded.dim]);
            }
            let len = ids.len();
            let ids = Tensor::new(ids, &loaded.device)
                .map_err(|e| e.to_string())?
                .unsqueeze(0)
                .map_err(|e| e.to_string())?;
            let mask =
                Tensor::ones((1, len), DType::U32, &loaded.device).map_err(|e| e.to_string())?;
            let token_types =
                Tensor::zeros((1, len), DType::U32, &loaded.device).map_err(|e| e.to_string())?;

            let out = Self::forward(loaded, &ids, &mask, &token_types)?;
            let pooled = Self::pool(loaded, &out, &mask)?; // (1, hidden)
            let vec: Vec<f32> = Self::normalize(&pooled)?
                .squeeze(0)
                .map_err(|e| e.to_string())?
                .to_vec1()
                .map_err(|e| e.to_string())?;
            Ok(vec)
        })();
        embed.map_err(|e| SlcError::Storage(format!("candle embed: {e}")))
    }

    /// Синхронный БАТЧ-эмбеддинг: один forward на все тексты (паддинг до
    /// максимальной длины в батче). Для индексации/реиндекса — на порядок
    /// быстрее последовательных вызовов. Только из `spawn_blocking`.
    fn embed_batch(
        loaded: &Loaded,
        texts: &[String],
        kind: EmbeddingKind,
    ) -> SlcResult<Vec<Vec<f32>>> {
        let embed = (|| -> Result<Vec<Vec<f32>>, String> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            // Токенизация + префиксы e5.
            let mut encodings: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
            for t in texts {
                let t = match (loaded.e5_prefix, kind) {
                    (true, EmbeddingKind::Query) => format!("query: {t}"),
                    (true, EmbeddingKind::Passage) => format!("passage: {t}"),
                    _ => t.clone(),
                };
                let enc = loaded
                    .tokenizer
                    .encode(t, true)
                    .map_err(|e| format!("tokenize: {e}"))?;
                encodings.push(enc.get_ids().to_vec());
            }
            let max_len = encodings.iter().map(|v| v.len()).max().unwrap_or(1).max(1);
            let batch = encodings.len();

            // Плотный паддинг + attention-маска.
            let mut ids: Vec<u32> = Vec::with_capacity(batch * max_len);
            let mut mask: Vec<u32> = Vec::with_capacity(batch * max_len);
            for e in &encodings {
                let pad = max_len - e.len();
                ids.extend(e.iter());
                ids.resize(ids.len() + pad, loaded.pad_id);
                mask.resize(mask.len() + e.len(), 1u32);
                mask.resize(mask.len() + pad, 0u32);
            }
            let ids = Tensor::new(ids, &loaded.device)
                .map_err(|e| e.to_string())?
                .reshape((batch, max_len))
                .map_err(|e| e.to_string())?;
            let mask_t = Tensor::new(mask, &loaded.device)
                .map_err(|e| e.to_string())?
                .reshape((batch, max_len))
                .map_err(|e| e.to_string())?;
            let token_types = Tensor::zeros((batch, max_len), DType::U32, &loaded.device)
                .map_err(|e| e.to_string())?;

            let out = Self::forward(loaded, &ids, &mask_t, &token_types)?; // (batch, seq, hidden)
            let pooled = Self::pool(loaded, &out, &mask_t)?; // (batch, hidden)
            let normed = Self::normalize(&pooled)?;
            normed.to_vec2().map_err(|e| e.to_string())
        })();
        embed.map_err(|e| SlcError::Storage(format!("candle embed batch: {e}")))
    }

    /// Общий forward: bert/xlm-roberta → (batch, seq, hidden).
    fn forward(
        loaded: &Loaded,
        ids: &Tensor,
        mask: &Tensor,
        token_types: &Tensor,
    ) -> Result<Tensor, String> {
        match &loaded.model {
            EncodeModel::Bert(m) => m
                .forward(ids, token_types, Some(mask))
                .map_err(|e| e.to_string()),
            EncodeModel::XlmRoberta(m) => m
                .forward(ids, mask, token_types, None, None, None)
                .map_err(|e| e.to_string()),
        }
    }

    /// Pooling → (batch, hidden): CLS (первый токен) или маскированный mean.
    fn pool(loaded: &Loaded, out: &Tensor, mask: &Tensor) -> Result<Tensor, String> {
        match loaded.pooling {
            Pooling::Cls => out
                .narrow(1, 0, 1)
                .map_err(|e| e.to_string())?
                .squeeze(1)
                .map_err(|e| e.to_string()),
            Pooling::Mean => {
                // Сумма по реальным токенам / количество реальных токенов.
                let mf = mask
                    .to_dtype(DType::F32)
                    .map_err(|e| e.to_string())?
                    .unsqueeze(2)
                    .map_err(|e| e.to_string())?;
                let sums = out
                    .broadcast_mul(&mf)
                    .map_err(|e| e.to_string())?
                    .sum(1)
                    .map_err(|e| e.to_string())?;
                let counts = mf
                    .sum(1)
                    .map_err(|e| e.to_string())?
                    .clamp(1.0f32, f32::MAX)
                    .map_err(|e| e.to_string())?;
                sums.broadcast_div(&counts).map_err(|e| e.to_string())
            }
        }
    }

    /// L2-нормализация последней оси.
    fn normalize(pooled: &Tensor) -> Result<Tensor, String> {
        let norm = pooled
            .sqr()
            .map_err(|e| e.to_string())?
            .sum(1)
            .map_err(|e| e.to_string())?
            .unsqueeze(1)
            .map_err(|e| e.to_string())?
            .sqrt()
            .map_err(|e| e.to_string())?;
        pooled.broadcast_div(&norm).map_err(|e| e.to_string())
    }
}

/// Скачать модель эмбеддингов в HF-кэш. Вызывается ТОЛЬКО из `slc-mcp init`
/// (консольный визард развертывания) — в рантайме автоскачивание отключено.
pub fn download_embedding_model(repo_id: &str) -> Result<(), String> {
    let (owner, name) = repo_id
        .split_once('/')
        .ok_or_else(|| format!("model repo id must be owner/name, got: {repo_id}"))?;
    let client = hf_hub::HFClientSync::new().map_err(|e| format!("hf-hub: {e}"))?;
    let repo = client.model(owner, name);
    // Веса: safetensors, а если репозиторий их не публикует (bge-m3) — torch.
    let weights = match repo
        .download_file()
        .filename(MODEL_FILES[0].to_string())
        .send()
    {
        Ok(p) => {
            tracing::info!(model = repo_id, file = MODEL_FILES[0], "downloaded");
            p
        }
        Err(_) => {
            tracing::info!(
                model = repo_id,
                file = PTH_FILE,
                "no safetensors — downloading torch weights"
            );
            repo.download_file()
                .filename(PTH_FILE.to_string())
                .send()
                .map_err(|e| format!("download {PTH_FILE}: {e}"))?
        }
    };
    for file in [MODEL_FILES[1], MODEL_FILES[2]] {
        tracing::info!(model = repo_id, file, "downloading model file");
        repo.download_file()
            .filename(file.to_string())
            .send()
            .map_err(|e| format!("download {file}: {e}"))?;
    }
    let _ = weights;
    Ok(())
}

/// Человекочитаемое имя активного устройства (для `slc-mcp init`).
pub fn detect_device_name() -> String {
    match detect_device() {
        DeviceKind::Cuda => "CUDA (GPU)".into(),
        DeviceKind::Metal => "Metal (GPU)".into(),
        DeviceKind::Cpu => "CPU".into(),
        DeviceKind::Auto => "auto".into(),
    }
}

/// Проверка кэша по repo id (без чтения env) — для `slc-mcp init`.
pub fn model_is_cached(repo_id: &str) -> bool {
    let probe = CandleEmbeddingLlm {
        repo_id: repo_id.to_string(),
        device_kind: DeviceKind::Cpu,
        state: Arc::new(Mutex::new(LoadState::Loading)),
        notify: Arc::new(tokio::sync::Notify::new()),
    };
    probe.model_cached()
}

/// Detect the GPU type available at runtime (cheap, no downloads).
///
/// NOTE: the candle Metal backend lacks layer-norm (verified e2e — bge-m3 on
/// macOS fails with "no metal implementation for layer-norm"), so macOS is
/// treated as CPU here; candle runs on Accelerate-optimized CPU. CUDA is
/// used only when built with the `cuda` feature.
fn detect_device() -> DeviceKind {
    #[cfg(feature = "cuda")]
    {
        if Device::cuda_if_available(0).is_ok() {
            return DeviceKind::Cuda;
        }
    }
    DeviceKind::Cpu
}

fn detect_and_resolve_device() -> Result<Device, String> {
    match detect_device() {
        #[cfg(feature = "cuda")]
        DeviceKind::Cuda => Device::cuda_if_available(0).map_err(|e| format!("CUDA: {e}")),
        #[cfg(all(target_os = "macos", feature = "candle-emb"))]
        DeviceKind::Metal => Device::new_metal(0).map_err(|e| format!("Metal: {e}")),
        DeviceKind::Cpu => Ok(Device::Cpu),
        _ => Err("no compute device".into()),
    }
}

#[async_trait]
impl LlmClient for CandleEmbeddingLlm {
    async fn reason(&self, _prompt: &str) -> SlcResult<String> {
        Err(SlcError::Storage(
            "candle embedding model cannot reason — enable SLC_MCP_SAMPLING for client inference or point SLC_LLM at Ollama/LM Studio".into(),
        ))
    }

    async fn generate_embedding(&self, text: &str) -> SlcResult<Vec<f32>> {
        self.generate_embedding_kind(text, EmbeddingKind::Passage)
            .await
    }

    async fn generate_embedding_kind(
        &self,
        text: &str,
        kind: EmbeddingKind,
    ) -> SlcResult<Vec<f32>> {
        let loaded = self.wait_ready().await?;
        let text = text.to_string();
        // Инференс на blocking-пуле: candle-операции синхронные и
        // занимают десятки-сотни мс — tokio-поток не блокируется.
        tokio::task::spawn_blocking(move || Self::embed_loaded(&loaded, &text, kind))
            .await
            .map_err(|e| SlcError::Storage(format!("embed task panicked: {e}")))?
    }

    async fn generate_embeddings(
        &self,
        texts: &[String],
        kind: EmbeddingKind,
    ) -> SlcResult<Vec<Vec<f32>>> {
        let loaded = self.wait_ready().await?;
        let texts = texts.to_vec();
        tokio::task::spawn_blocking(move || Self::embed_batch(&loaded, &texts, kind))
            .await
            .map_err(|e| SlcError::Storage(format!("embed batch task panicked: {e}")))?
    }

    fn embedding_model_name(&self) -> String {
        format!(
            "candle:{}",
            self.repo_id.split('/').next_back().unwrap_or("embed")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn not_cached_fails_fast() {
        // Несуществующий репозиторий: модель не в кэше → быстрый Err
        // с подсказкой про init (никакого скачивания/сети).
        let llm = CandleEmbeddingLlm::with_config("nonexistent/owner-model-xyz", "cpu");
        let err = llm
            .generate_embedding("тест")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("slc-mcp init"),
            "hint must point to init: {err}"
        );
    }

    #[tokio::test]
    #[ignore = "требует модель в HF-кэше (slc-mcp init)"]
    async fn cached_model_embeds_via_blocking_pool() {
        let llm = CandleEmbeddingLlm::with_config("intfloat/multilingual-e5-small", "cpu");
        let v = llm
            .generate_embedding("проверка эмбеддинга")
            .await
            .expect("embed");
        assert_eq!(v.len(), 384);
        let norm: f32 = v.iter().map(|x| x * x).sum();
        assert!((norm - 1.0).abs() < 1e-3, "L2-normalized, got {norm}");
    }

    #[tokio::test]
    #[ignore = "требует модель в HF-кэше (slc-mcp init)"]
    async fn batch_embeddings_match_single() {
        let llm = CandleEmbeddingLlm::with_config("intfloat/multilingual-e5-small", "cpu");
        let texts = vec![
            "кофе чёрный".to_string(),
            "ракета летит на орбиту".to_string(),
            "коза даёт молоко".to_string(),
        ];
        let batch = llm
            .generate_embeddings(&texts, EmbeddingKind::Passage)
            .await
            .expect("batch embed");
        assert_eq!(batch.len(), 3);
        for (b, t) in batch.iter().zip(&texts) {
            assert_eq!(b.len(), 384);
            let norm: f32 = b.iter().map(|x| x * x).sum();
            assert!((norm - 1.0).abs() < 1e-3, "L2-normalized, got {norm}");
            let single = llm
                .generate_embedding_kind(t, EmbeddingKind::Passage)
                .await
                .unwrap();
            let max_diff = b
                .iter()
                .zip(&single)
                .map(|(x, y)| (x - y).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_diff < 1e-3,
                "batch must match single, max diff {max_diff}"
            );
        }
    }
}
