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
use std::sync::{Arc, Condvar, Mutex};

/// GPU-модель по умолчанию: мультиязычная (включая русский), 1024-мер.
pub const BGE_M3: &str = "BAAI/bge-m3";
/// CPU-модель по умолчанию: лёгкая, 384-мер, тоже мультиязычная.
pub const E5_SMALL: &str = "intfloat/multilingual-e5-small";

/// Файлы модели, необходимые для загрузки.
const MODEL_FILES: [&str; 3] = ["model.safetensors", "config.json", "tokenizer.json"];

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
}

enum LoadState {
    Loading,
    Ready(Arc<Loaded>),
    Failed(String),
}

/// Onboard embedding client (candle). The model is loaded from the HF cache
/// in the background; embedding calls block until it is ready. If the model
/// was never downloaded (`slc-mcp init`), calls fail fast instead.
pub struct CandleEmbeddingLlm {
    repo_id: String,
    device_kind: DeviceKind,
    state: Mutex<LoadState>,
    cond: Condvar,
}

impl CandleEmbeddingLlm {
    /// Resolve the device kind from `SLC_EMBED_DEVICE` (auto-detected
    /// otherwise) and pick the model: GPU → bge-m3, CPU → e5-small.
    /// `SLC_EMBED_MODEL` overrides the model for any device.
    ///
    /// No network traffic happens here: the model is only loaded if it is
    /// already in the Hugging Face cache; otherwise the state is set to
    /// `Failed` with a pointer to `slc-mcp init`.
    pub fn new() -> Self {
        let device_kind = match std::env::var("SLC_EMBED_DEVICE").as_deref() {
            Ok("cuda") => DeviceKind::Cuda,
            Ok("metal") => DeviceKind::Metal,
            Ok("cpu") => DeviceKind::Cpu,
            Ok("auto") | Ok("") | Err(_) => detect_device(),
            Ok(other) => {
                tracing::warn!(device = other, "unknown SLC_EMBED_DEVICE — auto-detecting");
                detect_device()
            }
        };
        let repo_id = std::env::var("SLC_EMBED_MODEL").unwrap_or_else(|_| match device_kind {
            DeviceKind::Cuda | DeviceKind::Metal => BGE_M3.to_string(),
            DeviceKind::Auto | DeviceKind::Cpu => E5_SMALL.to_string(),
        });
        let llm = Self { repo_id, device_kind, state: Mutex::new(LoadState::Loading), cond: Condvar::new() };
        if llm.model_cached() {
            // Фоновая загрузка из кэша: старт сервера не блокируется.
            let worker = llm.clone_for_loader();
            std::thread::Builder::new()
                .name("slc-embed-loader".into())
                .spawn(move || worker.load_in_background())
                .expect("spawn slc-embed-loader");
        } else {
            tracing::warn!(
                model = %llm.repo_id,
                "embedding model is not downloaded — run `slc-mcp init` (auto-download is disabled)"
            );
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
        MODEL_FILES.iter().all(|file| {
            repo.download_file()
                .filename(file.to_string())
                .local_files_only(true)
                .send()
                .is_ok()
        })
    }

    /// Default model for a device kind (what `slc-mcp init` proposes).
    pub fn default_model_for(device_kind: &str) -> String {
        match device_kind {
            "cuda" | "metal" => BGE_M3.to_string(),
            _ => E5_SMALL.to_string(),
        }
    }

    /// Cheap copy used only by the background loader (thread-safety of the
    /// shared state is enough; repo_id/device_kind are Copy/owned).
    fn clone_for_loader(&self) -> Self {
        Self {
            repo_id: self.repo_id.clone(),
            device_kind: self.device_kind,
            state: Mutex::new(LoadState::Loading),
            cond: Condvar::new(),
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
        self.cond.notify_all();
    }

    fn wait_ready(&self) -> Result<Arc<Loaded>, SlcError> {
        let mut state = self.state.lock().unwrap();
        loop {
            match &*state {
                LoadState::Ready(l) => return Ok(l.clone()),
                LoadState::Failed(e) => {
                    return Err(SlcError::Storage(format!("embedding model unavailable: {e}")))
                }
                LoadState::Loading => state = self.cond.wait(state).unwrap(),
            }
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
        let model_path = dl("model.safetensors")?;
        let config_path = dl("config.json")?;
        let tokenizer_path = dl("tokenizer.json")?;

        let config_json = std::fs::read_to_string(config_path).map_err(|e| format!("read config: {e}"))?;
        let cfg: serde_json::Value = serde_json::from_str(&config_json).map_err(|e| format!("parse config: {e}"))?;
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

        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[model_path], DType::F32, &device) }
            .map_err(|e| format!("load safetensors: {e}"))?;
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
        let pooling = if self.repo_id.contains("bge-m3") { Pooling::Cls } else { Pooling::Mean };
        let e5_prefix = self.repo_id.contains("e5");

        let model = if architecture == "XlmRobertaModel" {
            let config: candle_transformers::models::xlm_roberta::Config =
                serde_json::from_str(&config_json).map_err(|e| format!("parse xlm config: {e}"))?;
            let m = candle_transformers::models::xlm_roberta::XLMRobertaModel::new(&config, vb)
                .map_err(|e| format!("load xlm-roberta: {e}"))?;
            EncodeModel::XlmRoberta(m)
        } else {
            let config: candle_transformers::models::bert::Config =
                serde_json::from_str(&config_json).map_err(|e| format!("parse bert config: {e}"))?;
            let m = candle_transformers::models::bert::BertModel::load(vb, &config)
                .map_err(|e| format!("load bert: {e}"))?;
            EncodeModel::Bert(m)
        };

        Ok(Loaded { model, tokenizer, device, dim, pooling, e5_prefix })
    }

    fn embed_loaded(&self, loaded: &Loaded, text: &str, kind: EmbeddingKind) -> SlcResult<Vec<f32>> {
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
            let mask = Tensor::ones((1, len), DType::U32, &loaded.device).map_err(|e| e.to_string())?;
            let token_types =
                Tensor::zeros((1, len), DType::U32, &loaded.device).map_err(|e| e.to_string())?;

            let out: Tensor = match &loaded.model {
                EncodeModel::Bert(m) => m.forward(&ids, &token_types, Some(&mask)).map_err(|e| e.to_string())?,
                EncodeModel::XlmRoberta(m) => {
                    m.forward(&ids, &mask, &token_types, None, None, None).map_err(|e| e.to_string())?
                }
            };
            let hidden = out.get(0).map_err(|e| e.to_string())?; // (seq, hidden)
            let pooled = match loaded.pooling {
                Pooling::Cls => hidden.get(0).map_err(|e| e.to_string())?,
                Pooling::Mean => hidden.mean(0).map_err(|e| e.to_string())?,
            };
            let norm = pooled
                .sqr()
                .map_err(|e| e.to_string())?
                .sum_all()
                .map_err(|e| e.to_string())?
                .sqrt()
                .map_err(|e| e.to_string())?;
            let vec: Vec<f32> = (pooled / norm)
                .map_err(|e| e.to_string())?
                .to_vec1()
                .map_err(|e| e.to_string())?;
            Ok(vec)
        })();
        embed.map_err(|e| SlcError::Storage(format!("candle embed: {e}")))
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
    for file in MODEL_FILES {
        tracing::info!(model = repo_id, file, "downloading model file");
        repo.download_file()
            .filename(file.to_string())
            .send()
            .map_err(|e| format!("download {file}: {e}"))?;
    }
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
        state: Mutex::new(LoadState::Loading),
        cond: Condvar::new(),
    };
    probe.model_cached()
}

/// Detect the GPU type available at runtime (cheap, no downloads).
fn detect_device() -> DeviceKind {    #[cfg(feature = "cuda")]
    {
        if Device::cuda_if_available(0).is_ok() {
            return DeviceKind::Cuda;
        }
    }
    #[cfg(all(target_os = "macos", feature = "candle-emb"))]
    {
        if Device::new_metal(0).is_ok() {
            return DeviceKind::Metal;
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
        let loaded = self.wait_ready()?;
        self.embed_loaded(&loaded, text, EmbeddingKind::Passage)
    }

    async fn generate_embedding_kind(&self, text: &str, kind: EmbeddingKind) -> SlcResult<Vec<f32>> {
        let loaded = self.wait_ready()?;
        self.embed_loaded(&loaded, text, kind)
    }

    fn embedding_model_name(&self) -> String {
        format!("candle:{}", self.repo_id.split('/').next_back().unwrap_or("embed"))
    }
}
