//! ReflectionEngine — periodic analysis of work history → actionable focuses.
//!
//! Legacy equivalent: `src/reflection/engine.py`. Runs as a TimerRegistry
//! handler for the `REFLECTION` timer: loads recent episodic history + active
//! focuses for a seat, asks the LLM for a JSON array of up to 5 focus
//! proposals, then adds each to the focus manager. Ideas as a concept were
//! dropped — focuses are the single proactive channel.

use crate::error::SlcResult;
use crate::focus::FocusManager;
use crate::llm::LlmClient;
use crate::model::{Document, PersistedTimer};
use crate::storage::{DocFilter, DocSort, SortDir, StorageBackend};

/// System prompt for the reflection LLM call.
pub const REFLECTION_SYSTEM_PROMPT: &str = "\
You are a background reflection engine for a knowledge management system.
Analyze the recent work history and current focuses provided below.
Identify non-obvious patterns, connections, potential improvements, and
actionable focus proposals worth keeping the agent's attention on.

Return a JSON array of focus objects:
[{\"content\": \"focus text\"}, ...]

Keep each focus concise (1-2 sentences). Return at most 5 focuses.
If nothing interesting stands out, return an empty array: []";

/// Max focuses the LLM may return.
pub const MAX_FOCUSES_PER_REFLECTION: usize = 5;

/// Runs background reflection for a seat and funnels focus proposals into
/// the focus manager.
#[derive(Clone)]
pub struct ReflectionEngine<S: StorageBackend, L: LlmClient> {
    store: S,
    llm: L,
}

impl<S: StorageBackend + Clone, L: LlmClient> ReflectionEngine<S, L> {
    pub fn new(store: S, llm: L) -> Self {
        ReflectionEngine { store, llm }
    }

    /// The `REFLECTION` timer handler. Loads history + focuses, generates
    /// focus proposals, and adds them. Skips silently when there is no
    /// recent history.
    pub async fn handle_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
        let seat_id = timer.seat_id.as_str();
        let history = self.load_recent_history(seat_id, 10).await?;
        if history.is_empty() {
            tracing::debug!("reflection [{seat_id}]: no recent history — skipping");
            return Ok(());
        }
        let focuses = self.load_focuses(seat_id).await?;
        let prompt = self.build_prompt(&history, &focuses);
        let raw = self.generate(prompt).await?;
        let proposals = parse_focuses(&raw);
        let mut added = 0;
        let fm = FocusManager::new(self.store.clone());
        for text in proposals {
            match fm.add(seat_id, &text, "", 1, &[], Some("shared")).await {
                Ok(_) => added += 1,
                Err(_) => break, // focus list full (MAX_FOCUSES)
            }
        }
        tracing::info!("reflection [{seat_id}]: {added} focuses added");
        Ok(())
    }

    /// Recent episodic HISTORY docs (prefer L2/L3 if present) for the seat.
    pub async fn load_recent_history(&self, seat_id: &str, limit: usize) -> SlcResult<Vec<Document>> {
        let filter = DocFilter { seat_id: Some(seat_id.into()), category: None, ..Default::default() };
        let mut docs = self.store.episodic_find(&filter, &DocSort::by_created(SortDir::Desc), limit).await?;
        // Prefer higher levels: sort L2/L3 before raw L1 within the window.
        docs.sort_by(|a, b| level_rank(b).cmp(&level_rank(a)));
        Ok(docs.into_iter().take(limit).collect())
    }

    /// Active focuses for the seat (title + priority).
    pub async fn load_focuses(&self, seat_id: &str) -> SlcResult<Vec<(String, i64)>> {
        let focuses = FocusManager::new(self.store.clone()).get_active(seat_id, None).await?;
        Ok(focuses.into_iter().map(|f| (f.title, f.priority)).collect())
    }

    fn build_prompt(&self, history: &[Document], focuses: &[(String, i64)]) -> String {
        let mut parts = vec!["## Recent Work History".to_string()];
        for d in history {
            let summary = d.metadata.extra.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !summary.is_empty() {
                summary.to_string()
            } else {
                d.content.chars().take(200).collect::<String>()
            };
            parts.push(format!("- [{}] {body}", d.created_at));
        }
        if !focuses.is_empty() {
            parts.push("\n## Current Focuses".into());
            for (title, priority) in focuses {
                parts.push(format!("- [{priority}] {title}"));
            }
        }
        parts.push("\nAnalyze the above and generate actionable focus proposals as a JSON array.".into());
        parts.join("\n")
    }

    async fn generate(&self, prompt: String) -> SlcResult<String> {
        self.llm.reason(&prompt).await
    }
}

/// Extract focus texts from an LLM response (JSON array of `{content}`).
pub fn parse_focuses(raw: &str) -> Vec<String> {
    let start = raw.find('[');
    let end = raw.rfind(']');
    let (Some(start), Some(end)) = (start, end) else { return vec![] };
    if end <= start {
        return vec![];
    }
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw[start..=end]) else {
        return vec![];
    };
    let Some(arr) = data.as_array() else { return vec![] };
    arr.iter()
        .filter_map(|item| {
            if let Some(obj) = item.as_object() {
                obj.get("content").and_then(|c| c.as_str()).map(String::from)
            } else {
                item.as_str().map(String::from)
            }
        })
        .take(MAX_FOCUSES_PER_REFLECTION)
        .collect()
}
fn level_rank(d: &Document) -> i64 {
    match d.metadata.doc_level {
        Some(crate::model::DocLevel::L2) => 2,
        Some(crate::model::DocLevel::L3) => 3,
        Some(crate::model::DocLevel::L4) => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockLlm;
    use crate::model::{DocLevel, DocMeta, Document, DocumentCategory};
    use crate::storage::sqlite::SqliteStore;
    use std::sync::Arc;

    #[tokio::test]
    async fn reflection_adds_focuses_from_llm() {
        let llm = MockLlm::new(vec!["[{\"content\": \"improve latency\"}, {\"content\": \"add tests\"}]".to_string()]);
        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let mut meta = DocMeta::default();
        meta.doc_type = Some("EPISODIC".into());
        meta.doc_level = Some(DocLevel::L1);
        meta.seat_id = Some("seat_r".into());
        let doc = Document::new("ev_1", DocumentCategory::History, "worked on audio pipeline", meta, vec![], Some("seat_r".into()));
        store.episodic_insert(&doc).await.unwrap();
        let engine = ReflectionEngine::new(store.clone(), llm);

        let timer = crate::model::PersistedTimer {
            timer_id: "t1".into(),
            seat_id: "seat_r".into(),
            timer_type: crate::model::TimerType::Reflection,
            interval_seconds: Some(7200),
            last_fired_at: None,
            next_fire_at: chrono::Utc::now(),
            is_active: true,
            metadata: Default::default(),
            created_at: chrono::Utc::now(),
        };
        engine.handle_timer(&timer).await.unwrap();
        let focuses = FocusManager::new(store.clone()).get_active("seat_r", None).await.unwrap();
        assert_eq!(focuses.len(), 2);
        assert!(focuses.iter().any(|f| f.title == "improve latency"));
        assert!(focuses.iter().any(|f| f.title == "add tests"));
    }

    #[tokio::test]
    async fn reflection_skips_without_history() {
        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let engine = ReflectionEngine::new(store.clone(), MockLlm::default());
        let timer = crate::model::PersistedTimer {
            timer_id: "t".into(),
            seat_id: "empty_seat".into(),
            timer_type: crate::model::TimerType::Reflection,
            interval_seconds: Some(7200),
            last_fired_at: None,
            next_fire_at: chrono::Utc::now(),
            is_active: true,
            metadata: Default::default(),
            created_at: chrono::Utc::now(),
        };
        engine.handle_timer(&timer).await.unwrap();
        let focuses = FocusManager::new(store).get_active("empty_seat", None).await.unwrap();
        assert!(focuses.is_empty());
    }

    #[test]
    fn parse_focuses_handles_plain_and_json() {
        let parsed = parse_focuses("[{\"content\": \"one\"}, \"two\", {\"content\": \"three\"}]");
        assert_eq!(parsed, vec!["one".to_string(), "two".to_string(), "three".to_string()]);
        assert!(parse_focuses("no brackets here").is_empty());
        assert!(parse_focuses("[]").is_empty());
        // >5 truncated
        let many = (0..7).map(|i| format!("{{\"content\": \"{i}\"}}")).collect::<Vec<_>>().join(",");
        assert_eq!(parse_focuses(&format!("[{many}]")).len(), 5);
    }
}
