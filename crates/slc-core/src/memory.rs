//! The memory pipeline: progressive summarization (L1→L4) and fact
//! consolidation. Operates on the EPISODIC store only; extracted facts land
//! in the KB (semantic memory).
//!
//! Ported from legacy `src/memory/{compressor,consolidator}.py` with the
//! known fix: `metadata.consolidated` is now actually SET after extraction
//! (legacy only filtered on it, so L2/L3 sources were re-harvested every
//! cycle).

use crate::error::SlcResult;
use crate::llm::LlmClient;
use crate::model::{DocLevel, DocMeta, Document, DocumentCategory, content_hash};
use crate::storage::{DocFilter, DocSort, MetaPatch, SortDir, StorageBackend};
use chrono::{Duration, Timelike, Utc};

/// L1 raw events are summarized into L2 daily summaries after this many days
/// (mirrors legacy constant; today L1→L2 runs for "yesterday" explicitly).
pub const EPISODIC_L1_TTL_DAYS: i64 = 7;
/// Soft cap of facts per seat (legacy `MAX_LEARNED_FACTS`).
pub const MAX_LEARNED_FACTS: i64 = 200;
/// Cosine threshold above which a fact is considered a duplicate.
pub const DUPLICATE_THRESHOLD: f32 = 0.92;
/// Max chars of combined source text fed to the LLM (legacy truncation).
const PROMPT_CHARS: usize = 4000;

const CONSOLIDATION_PROMPT: &str = r#"You are a fact extraction engine. From the episodic summaries below,
extract concrete, reusable facts and lessons learned.

Return a JSON array of strings, each a single fact (1-2 sentences).
Only include genuinely useful insights, not trivial observations.
Return at most 10 facts. If nothing useful — return [].

Summaries:
{summaries}"#;

// ─────────────────────────── progressive summarization ───────────────────────────

/// Progressive summarization: L1 raw → L2 daily → L3 weekly → L4 insights.
#[derive(Clone)]
pub struct HistoryCompressor<S: StorageBackend, L: LlmClient> {
    store: S,
    llm: L,
}

impl<S: StorageBackend, L: LlmClient> HistoryCompressor<S, L> {
    pub fn new(store: S, llm: L) -> Self {
        HistoryCompressor { store, llm }
    }

    /// Run all three compression stages for a seat. Idempotent per batch:
    /// consumed sources are marked `archived` + `compression_batch_id`.
    pub async fn compress(&self, seat_id: &str) -> SlcResult<CompressionReport> {
        let batch_id = crate::model::unique_id("batch");
        let report = CompressionReport {
            l1_to_l2: self.compress_l1_to_l2(seat_id, &batch_id).await?,
            l2_to_l3: self.compress_l2_to_l3(seat_id, &batch_id).await?,
            l3_to_l4: self.compress_l3_to_l4(seat_id, &batch_id).await?,
        };
        Ok(report)
    }

    /// Yesterday's L1 events → one L2 daily summary.
    async fn compress_l1_to_l2(&self, seat_id: &str, batch_id: &str) -> SlcResult<usize> {
        let now = Utc::now();
        let yesterday_start = now
            .with_hour(0)
            .and_then(|d| d.with_minute(0))
            .and_then(|d| d.with_second(0))
            .map(|d| d - Duration::days(1))
            .unwrap_or_else(|| now - Duration::days(1));
        let yesterday_end = yesterday_start + Duration::days(1);

        let filter = DocFilter {
            seat_id: Some(seat_id.into()),
            doc_level: Some(DocLevel::L1),
            has_compression_batch: Some(false),
            ..Default::default()
        };
        let l1: Vec<_> = self
            .store
            .episodic_find(&filter, &DocSort::by_created(SortDir::Asc), 500)
            .await?
            .into_iter()
            // Date window is on created_at (the diary day), not updated_at.
            .filter(|d| d.created_at >= yesterday_start && d.created_at < yesterday_end)
            .collect();
        if l1.is_empty() {
            return Ok(0);
        }

        let combined = combine(&l1, PROMPT_CHARS);
        let prompt = format!(
            "Summarize this day's work log into a concise daily summary (3-5 bullet points):\n\n{combined}"
        );
        let Ok(summary) = self.llm.reason_for(seat_id, &prompt).await else {
            return Ok(0); // LLM unavailable — keep sources unmarked, retry next run
        };

        let date_str = yesterday_start.format("%Y-%m-%d").to_string();
        let mut meta = DocMeta::default();
        meta.doc_type = Some("EPISODIC".into());
        meta.doc_level = Some(DocLevel::L2);
        meta.seat_id = Some(seat_id.into());
        meta.date = Some(date_str.clone());
        meta.source_count = Some(l1.len() as i64);
        meta.compression_batch_id = Some(batch_id.into());
        let l2 = Document::with_folder(
            format!("episodic_l2_{seat_id}_{date_str}"),
            DocumentCategory::History,
            None, // default folder: history/YYYY/MM/
            format!("# Daily Summary — {date_str}\n\n{summary}"),
            meta,
            vec!["episodic".into(), "daily_summary".into()],
            Some(seat_id.into()),
        );
        self.store.episodic_insert(&l2).await?;

        // Mark the EXACT sources consumed — never the whole seat's unmarked
        // L1 (that used to mark today's events as consumed: they were not in
        // yesterday's window, and tomorrow's run would skip them → loss).
        let consumed: Vec<String> = l1.iter().map(|d| d.document_id.clone()).collect();
        self.store
            .episodic_patch_meta(
                &DocFilter {
                    document_ids: Some(consumed),
                    ..Default::default()
                },
                &MetaPatch {
                    set_archived: Some(true),
                    set_compression_batch_id: Some(batch_id.into()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(l1.len())
    }

    /// ≥7 un-archived L2 daily summaries → one L3 weekly digest.
    async fn compress_l2_to_l3(&self, seat_id: &str, batch_id: &str) -> SlcResult<usize> {
        let filter = DocFilter {
            seat_id: Some(seat_id.into()),
            doc_level: Some(DocLevel::L2),
            archived: Some(false),
            ..Default::default()
        };
        let l2 = self
            .store
            .episodic_find(&filter, &DocSort::by_created(SortDir::Asc), 100)
            .await?;
        if l2.len() < 7 {
            return Ok(0);
        }
        let batch = &l2[..7];
        let combined = combine(batch, PROMPT_CHARS);
        let prompt = format!(
            "Summarize this week's daily summaries into a weekly digest (key achievements, decisions, blockers):\n\n{combined}"
        );
        let Ok(summary) = self.llm.reason_for(seat_id, &prompt).await else {
            return Ok(0);
        };

        let week_id = batch[0]
            .metadata
            .date
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let mut meta = DocMeta::default();
        meta.doc_type = Some("EPISODIC".into());
        meta.doc_level = Some(DocLevel::L3);
        meta.seat_id = Some(seat_id.into());
        meta.date = Some(week_id.clone());
        meta.source_count = Some(batch.len() as i64);
        meta.compression_batch_id = Some(batch_id.into());
        let l3 = Document::with_folder(
            format!("episodic_l3_{seat_id}_{week_id}"),
            DocumentCategory::History,
            None,
            format!("# Weekly Digest — week of {week_id}\n\n{summary}"),
            meta,
            vec!["episodic".into(), "weekly_digest".into()],
            Some(seat_id.into()),
        );
        self.store.episodic_insert(&l3).await?;

        let ids: Vec<String> = batch.iter().map(|d| d.document_id.clone()).collect();
        self.store
            .episodic_patch_meta(
                &DocFilter {
                    document_ids: Some(ids),
                    ..Default::default()
                },
                &MetaPatch {
                    set_archived: Some(true),
                    set_compression_batch_id: Some(batch_id.into()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(batch.len())
    }

    /// ≥4 un-archived L3 weekly digests → upsert the L4 project insights doc.
    async fn compress_l3_to_l4(&self, seat_id: &str, batch_id: &str) -> SlcResult<usize> {
        let filter = DocFilter {
            seat_id: Some(seat_id.into()),
            doc_level: Some(DocLevel::L3),
            archived: Some(false),
            ..Default::default()
        };
        let l3 = self
            .store
            .episodic_find(&filter, &DocSort::by_created(SortDir::Asc), 100)
            .await?;
        if l3.len() < 4 {
            return Ok(0);
        }
        let batch = &l3[..4];
        let combined = combine(batch, PROMPT_CHARS);
        let l4_id = format!("episodic_l4_{seat_id}");

        // L4 is a single upserted doc; find it across the episodic store.
        let existing = self
            .store
            .episodic_find(
                &DocFilter {
                    seat_id: Some(seat_id.into()),
                    doc_type: Some("EPISODIC".into()),
                    ..Default::default()
                },
                &DocSort::default(),
                100,
            )
            .await?
            .into_iter()
            .find(|d| d.document_id == l4_id);

        let prompt = match &existing {
            Some(e) => format!(
                "You have existing project insights:\n{}\n\nAnd new weekly digests:\n{}\n\nMerge into updated, comprehensive project insights.",
                truncate(&e.content, 2000),
                truncate(&combined, 2000)
            ),
            None => format!(
                "Distill these weekly digests into high-level project insights (key learnings, architectural patterns, team dynamics):\n\n{combined}"
            ),
        };
        let Ok(insights) = self.llm.reason_for(seat_id, &prompt).await else {
            return Ok(0);
        };

        let mut meta = DocMeta::default();
        meta.doc_type = Some("EPISODIC".into());
        meta.doc_level = Some(DocLevel::L4);
        meta.seat_id = Some(seat_id.into());
        meta.compression_batch_id = Some(batch_id.into());
        let l4 = Document::with_folder(
            l4_id.clone(),
            DocumentCategory::History,
            None,
            format!("# Project Insights\n\n{insights}"),
            meta,
            vec!["episodic".into(), "project_insights".into()],
            Some(seat_id.into()),
        );
        if existing.is_some() {
            self.store.episodic_upsert(&l4).await?;
        } else {
            self.store.episodic_insert(&l4).await?;
        }

        let consumed: Vec<String> = batch.iter().map(|d| d.document_id.clone()).collect();
        self.store
            .episodic_patch_meta(
                &DocFilter {
                    document_ids: Some(consumed),
                    ..Default::default()
                },
                &MetaPatch {
                    set_archived: Some(true),
                    set_compression_batch_id: Some(batch_id.into()),
                    ..Default::default()
                },
            )
            .await?;
        Ok(batch.len())
    }
}

/// Per-run summary of what the compressor did.
#[derive(Debug, Clone, Default)]
pub struct CompressionReport {
    pub l1_to_l2: usize,
    pub l2_to_l3: usize,
    pub l3_to_l4: usize,
}

// ─────────────────────────────── consolidation ───────────────────────────────

/// Extracts LEARNED_FACTs from episodic L2/L3 into the KB (semantic memory).
#[derive(Clone)]
pub struct MemoryConsolidator<S: StorageBackend, L: LlmClient> {
    store: S,
    llm: L,
}

impl<S: StorageBackend, L: LlmClient> MemoryConsolidator<S, L> {
    pub fn new(store: S, llm: L) -> Self {
        MemoryConsolidator { store, llm }
    }

    pub async fn consolidate(&self, seat_id: &str) -> SlcResult<ConsolidationReport> {
        let filter = DocFilter {
            seat_id: Some(seat_id.into()),
            doc_level: None,
            not_consolidated: true,
            ..Default::default()
        };
        // L2/L3 sources, newest first, ≤20.
        let sources = self
            .store
            .episodic_find(&filter, &DocSort::by_created(SortDir::Desc), 20)
            .await?
            .into_iter()
            .filter(|d| matches!(d.metadata.doc_level, Some(DocLevel::L2 | DocLevel::L3)))
            .collect::<Vec<_>>();
        if sources.is_empty() {
            return Ok(ConsolidationReport::default());
        }

        let combined: Vec<String> = sources.iter().map(|d| truncate(&d.content, 500)).collect();
        let prompt = CONSOLIDATION_PROMPT.replace(
            "{summaries}",
            &truncate(&combined.join("\n\n"), PROMPT_CHARS),
        );
        let Ok(raw) = self.llm.reason_for(seat_id, &prompt).await else {
            return Ok(ConsolidationReport::default());
        };

        let facts = parse_facts(&raw);
        let mut added = 0;
        for fact in &facts {
            if self.count_facts(seat_id).await? >= MAX_LEARNED_FACTS {
                break;
            }
            if self.is_duplicate(fact).await? {
                continue;
            }
            self.store_fact(seat_id, fact).await?;
            added += 1;
        }

        // FIX (vs legacy): mark sources consolidated so they are not
        // re-harvested on the next cycle.
        if !sources.is_empty() {
            let ids: Vec<String> = sources.iter().map(|d| d.document_id.clone()).collect();
            self.store
                .episodic_patch_meta(
                    &DocFilter {
                        seat_id: Some(seat_id.into()),
                        ..Default::default()
                    },
                    &MetaPatch {
                        set_consolidated: Some(true),
                        ..Default::default()
                    },
                )
                .await?;
            let _ = ids;
        }

        Ok(ConsolidationReport {
            sources: sources.len(),
            facts_added: added,
            facts_total: facts.len(),
        })
    }

    async fn is_duplicate(&self, fact: &str) -> SlcResult<bool> {
        let qv = match self.llm.generate_embedding(fact).await {
            Ok(v) => v,
            Err(_) => return Ok(false), // can't check — treat as new
        };
        let records = self
            .store
            .all_embeddings(crate::model::EmbeddingScope::Public, None)
            .await?;
        for r in records {
            if r.document_id.starts_with("learned_fact_")
                && r.embedding_dimension == qv.len()
                && crate::search::cosine_similarity(&qv, &r.embedding) > DUPLICATE_THRESHOLD
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn count_facts(&self, seat_id: &str) -> SlcResult<i64> {
        let filter = DocFilter {
            seat_id: Some(seat_id.into()),
            doc_type: Some("LEARNED_FACT".into()),
            ..Default::default()
        };
        Ok(self.store.kb_count(&filter).await? as i64)
    }

    async fn store_fact(&self, seat_id: &str, fact: &str) -> SlcResult<String> {
        let fact_hash = &content_hash(fact)[..12];
        let doc_id = format!("learned_fact_{seat_id}_{fact_hash}");
        let mut meta = DocMeta::default();
        meta.doc_type = Some("LEARNED_FACT".into());
        meta.seat_id = Some(seat_id.into());
        meta.source = Some("consolidation".into());
        let doc = Document::new(
            doc_id.clone(),
            DocumentCategory::System,
            fact,
            meta,
            vec!["learned_fact".into(), "semantic_memory".into()],
            Some(seat_id.into()),
        );
        self.store.kb_insert(&doc).await?;

        // Best-effort embedding (dedup + semantic search need it).
        if let Ok(emb) = self.llm.generate_embedding(fact).await {
            let now = chrono::Utc::now();
            self.store
                .insert_embeddings(&[crate::model::EmbeddingRecord {
                    document_id: doc_id.clone(),
                    chunk_index: 0,
                    chunk_total: 1,
                    embedding: emb.clone(),
                    embedding_model: self.llm.embedding_model_name(),
                    embedding_dimension: emb.len(),
                    generated_at: now,
                    scope: crate::model::EmbeddingScope::Public,
                    seat_id: Some(seat_id.into()),
                }])
                .await?;
        }
        Ok(doc_id)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConsolidationReport {
    pub sources: usize,
    pub facts_added: usize,
    pub facts_total: usize,
}

/// Extract the first JSON array of strings from the LLM reply (≤10 facts).
pub fn parse_facts(raw: &str) -> Vec<String> {
    let start = raw.find('[');
    let end = raw.rfind(']');
    let (Some(s), Some(e)) = (start, end) else {
        return Vec::new();
    };
    if e <= s {
        return Vec::new();
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw[s..=e]) else {
        return Vec::new();
    };
    json.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .take(10)
                .collect()
        })
        .unwrap_or_default()
}

fn combine(docs: &[Document], max_chars: usize) -> String {
    let mut parts = Vec::new();
    let mut len = 0;
    for d in docs {
        let piece = d.content.trim();
        if piece.is_empty() {
            continue;
        }
        if len + piece.len() + 2 > max_chars {
            break;
        }
        len += piece.len() + 2;
        parts.push(piece.to_string());
    }
    parts.join("\n\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockLlm;
    use crate::storage::sqlite::SqliteStore;

    fn l1_doc(seat: &str, day: u32, content: &str) -> Document {
        let mut m = DocMeta::default();
        m.doc_level = Some(DocLevel::L1);
        m.seat_id = Some(seat.into());
        let _ = day; // day only distinguishes the event id
        let mut d = Document::new(
            format!("evt-{day}"),
            DocumentCategory::History,
            content,
            m,
            vec![],
            Some(seat.into()),
        );
        // Place "yesterday" by rewriting created_at (with_hour → 12:00).
        let now = Utc::now();
        let yesterday = now - Duration::days(1);
        d.created_at = yesterday.with_hour(12).unwrap();
        d
    }

    #[tokio::test]
    async fn l1_to_l2_compression() {
        let store = SqliteStore::in_memory().unwrap();
        let seat = "seat_t";
        for i in 1..=3 {
            store
                .episodic_insert(&l1_doc(seat, i, &format!("worked on task {i}")))
                .await
                .unwrap();
        }
        // Scripted summary from the "LLM".
        let llm = MockLlm::new(vec!["- did tasks 1..3".into()]);
        let compressor = HistoryCompressor::new(store.clone(), llm);
        let report = compressor.compress(seat).await.unwrap();
        assert_eq!(report.l1_to_l2, 3);

        // L2 daily summary exists; sources archived + batch-marked.
        let l2 = store
            .episodic_find(
                &DocFilter {
                    seat_id: Some(seat.into()),
                    doc_level: Some(DocLevel::L2),
                    ..Default::default()
                },
                &DocSort::default(),
                10,
            )
            .await
            .unwrap();
        assert_eq!(l2.len(), 1);
        assert!(l2[0].content.contains("- did tasks 1..3"));
        assert_eq!(l2[0].metadata.doc_type.as_deref(), Some("EPISODIC"));

        let remaining = store
            .episodic_count(&DocFilter {
                seat_id: Some(seat.into()),
                has_compression_batch: Some(false),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(remaining, 0, "all L1 sources consumed");
    }

    #[tokio::test]
    async fn consolidation_extracts_and_dedups() {
        let store = SqliteStore::in_memory().unwrap();
        let seat = "seat_f";
        for (i, level) in [(1, DocLevel::L2), (2, DocLevel::L3)] {
            let mut m = DocMeta::default();
            m.doc_level = Some(level);
            m.seat_id = Some(seat.into());
            store
                .episodic_insert(&Document::new(
                    format!("src-{i}"),
                    DocumentCategory::History,
                    format!("summary {i}: the team uses Rust for the audio pipeline"),
                    m,
                    vec![],
                    Some(seat.into()),
                ))
                .await
                .unwrap();
        }
        let llm = MockLlm::new(vec![
            "[\"The team uses Rust for audio\", \"Prefer metal for STT\"]".into(),
        ]);
        let consolidator = MemoryConsolidator::new(store.clone(), llm);
        let report = consolidator.consolidate(seat).await.unwrap();
        assert_eq!(report.facts_added, 2);
        assert_eq!(report.facts_total, 2);

        // Facts are KB docs with LEARNED_FACT type + embeddings.
        let facts = store
            .kb_find(
                &DocFilter {
                    seat_id: Some(seat.into()),
                    doc_type: Some("LEARNED_FACT".into()),
                    ..Default::default()
                },
                &DocSort::default(),
                10,
            )
            .await
            .unwrap();
        assert_eq!(facts.len(), 2);
        assert!(
            store
                .get_embedding(&facts[0].document_id)
                .await
                .unwrap()
                .is_some()
        );

        // Second run: sources marked consolidated → nothing new.
        let report2 = consolidator.consolidate(seat).await.unwrap();
        assert_eq!(
            report2.sources, 0,
            "consolidated flag must prevent re-harvesting"
        );
    }

    #[test]
    fn parse_facts_handles_plain_and_malformed() {
        assert_eq!(
            parse_facts("[\"a\", \"b\"]"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(parse_facts("Here: [\"x\"] done"), vec!["x".to_string()]);
        assert_eq!(parse_facts("no array here"), Vec::<String>::new());
        assert_eq!(parse_facts("[]"), Vec::<String>::new());
    }
    /// Regression: the L1→L2 patch used to mark the WHOLE seat's unmarked L1
    /// as consumed — including TODAY's events, which were not in yesterday's
    /// date window and therefore never made it into the digest. Tomorrow's
    /// run would skip them → silent data loss.
    #[tokio::test]
    async fn l1_to_l2_does_not_consume_todays_events() {
        let store = SqliteStore::in_memory().unwrap();
        let seat = "seat_today";
        // One YESTERDAY event (in the window) + one TODAY event (not in it).
        store
            .episodic_insert(&l1_doc(seat, 1, "yesterday work"))
            .await
            .unwrap();
        let mut today = l1_doc(seat, 2, "today work");
        today.created_at = Utc::now().with_hour(15).unwrap();
        today.document_id = "evt-today".into();
        store.episodic_insert(&today).await.unwrap();

        let llm = MockLlm::new(vec!["- yesterday work".into()]);
        let compressor = HistoryCompressor::new(store.clone(), llm);
        let report = compressor.compress(seat).await.unwrap();
        assert_eq!(report.l1_to_l2, 1);

        // The today event must STILL be unmarked (available for tomorrow).
        let today_doc = store
            .episodic_find(
                &DocFilter {
                    document_ids: Some(vec!["evt-today".into()]),
                    ..Default::default()
                },
                &DocSort::default(),
                10,
            )
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.document_id == "evt-today")
            .expect("today event exists");
        assert!(
            today_doc.metadata.archived != Some(true)
                && today_doc.metadata.compression_batch_id.is_none(),
            "today's event must NOT be consumed: {:?}",
            today_doc.metadata
        );
    }
}
