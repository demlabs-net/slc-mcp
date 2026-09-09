//! Hybrid search over the KNOWLEDGE BASE only — episodic history never
//! enters here by construction (it lives in the separate episodic store).
//!
//! Pipeline (ported from legacy `search.py`):
//! 1. candidate retrieval: semantic (cosine over embedding chunks, best
//!    score per doc) + text (BM25 over tokenized content),
//! 2. merge with weights (`semantic_weight`/`text_weight`),
//! 3. relevance gate: drop embedding noise (absolute floor
//!    `SLC_SEARCH_MIN_SCORE` + relative gap `SLC_SEARCH_MIN_GAP` × top),
//! 4. inverted-context rerank: relevance + importance + recency + mem_type
//!    (env-tunable weights).

use crate::error::SlcResult;
use crate::llm::LlmClient;
use crate::model::{Document, DocumentCategory, EmbeddingScope};
use crate::storage::{DocFilter, DocSort, SortDir, StorageBackend};
use std::collections::HashMap;

/// One search hit: the document + scores.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub document: Document,
    /// Hybrid score (0..1-ish) before rerank.
    pub score: f32,
    /// Blended rank score after the inverted reranker.
    pub rank_score: f32,
}

/// Text embedding similarity.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let mag = na.sqrt() * nb.sqrt();
    if mag <= f32::EPSILON { 0.0 } else { dot / mag }
}

/// Lowercase word tokens (unicode letters/digits runs).
pub fn tokenize(text: &str) -> Vec<String> {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().collect::<Vec<_>>()
            } else {
                vec![' ']
            }
        })
        .flatten()
        .collect::<String>()
        .split_whitespace()
        .map(String::from)
        .collect()
}

/// Term frequency map.
fn term_freq(tokens: &[String]) -> HashMap<String, usize> {
    let mut tf = HashMap::new();
    for t in tokens {
        *tf.entry(t.clone()).or_insert(0) += 1;
    }
    tf
}

/// BM25-ish text score: sum over query terms of `(1 + ln tf) * idf`,
/// length-normalized. IDF is computed over the candidate set.
fn bm25(
    query_terms: &[String],
    doc_tokens: &[String],
    df: &HashMap<String, usize>,
    total_docs: usize,
) -> f32 {
    let tf = term_freq(doc_tokens);
    let doc_len = doc_tokens.len().max(1) as f32;
    let avg_len = 200.0f32; // soft normalizer; candidates are short notes
    let k1 = 1.2f32;
    let b = 0.75f32;
    let mut score = 0.0f32;
    let mut seen = std::collections::HashSet::new();
    for term in query_terms {
        if !seen.insert(term.clone()) {
            continue;
        }
        let n = df.get(term).copied().unwrap_or(0);
        let idf = ((total_docs.max(1) as f32 - n as f32 + 0.5) / (n as f32 + 0.5) + 1.0).ln();
        let f = *tf.get(term).unwrap_or(&0) as f32;
        score += idf * (f * (k1 + 1.0)) / (f + k1 * (1.0 - b + b * doc_len / avg_len));
    }
    score
}

/// Reranker weights (env-tunable, mirroring legacy RankWeights).
#[derive(Debug, Clone, Copy)]
pub struct RankWeights {
    pub enabled: bool,
    pub relevance: f32,
    pub importance: f32,
    pub recency: f32,
    pub mem_type: f32,
    pub recency_half_life_days: f32,
}

impl Default for RankWeights {
    fn default() -> Self {
        RankWeights {
            enabled: std::env::var("SLC_SEARCH_RANK_INVERSION").is_ok_and(|v| v == "1"),
            relevance: env_f("SLC_RANK_W_RELEVANCE", 1.0),
            importance: env_f("SLC_RANK_W_IMPORTANCE", 0.30),
            recency: env_f("SLC_RANK_W_RECENCY", 0.20),
            mem_type: env_f("SLC_RANK_W_TYPE", 0.10),
            recency_half_life_days: env_f("SLC_RANK_RECENCY_HALFLIFE_DAYS", 30.0),
        }
    }
}

fn env_f(key: &str, def: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(def)
}

fn importance(doc: &Document) -> f32 {
    if let Some(imp) = doc.metadata.importance {
        return imp.clamp(0.0, 1.0) as f32;
    }
    for tag in &doc.tags {
        if let Some(prio) = tag.strip_prefix("priority:") {
            return match prio {
                "low" => 0.25,
                "medium" => 0.5,
                "high" => 0.75,
                "critical" => 1.0,
                _ => 0.5,
            };
        }
    }
    0.5
}

fn mem_type(doc: &Document) -> f32 {
    for tag in &doc.tags {
        if let Some(t) = tag.strip_prefix("memory:") {
            return match t {
                "procedural" => 1.0,
                "semantic" => 0.75,
                "episodic" => 0.5,
                "working" => 0.25,
                _ => 0.5,
            };
        }
    }
    0.5
}

fn recency(doc: &Document, half_life_days: f32) -> f32 {
    let now = chrono::Utc::now();
    let ts = doc.updated_at.max(doc.created_at);
    let age_days = (now - ts).num_seconds() as f32 / 86400.0;
    (-age_days / half_life_days.max(0.001)).exp()
}

/// Apply the inverted-context reranker: blend relevance (min-max normalized
/// across the set), importance, recency and memory-type into `rank_score`,
/// sort desc, truncate to `limit`.
pub fn rerank_inverted(
    mut hits: Vec<SearchHit>,
    weights: &RankWeights,
    limit: usize,
) -> Vec<SearchHit> {
    if !weights.enabled || hits.len() <= 1 {
        hits.truncate(limit);
        return hits;
    }
    let rel: Vec<f32> = hits.iter().map(|h| h.score).collect();
    let min = rel.iter().cloned().fold(f32::INFINITY, f32::min);
    let max = rel.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let range = max - min;
    for (idx, h) in hits.iter_mut().enumerate() {
        let relevance = if range <= f32::EPSILON {
            1.0
        } else {
            (rel[idx] - min) / range
        };
        let imp = importance(&h.document);
        let rec = recency(&h.document, weights.recency_half_life_days);
        let mt = mem_type(&h.document);
        h.rank_score = weights.relevance * relevance
            + weights.importance * imp
            + weights.recency * rec
            + weights.mem_type * mt;
    }
    hits.sort_by(|a, b| {
        b.rank_score
            .partial_cmp(&a.rank_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

/// Hybrid search service over the KB store.
pub struct SearchService {
    store: std::sync::Arc<dyn StorageBackend>,
    llm: std::sync::Arc<dyn LlmClient>,
    semantic_weight: f32,
    text_weight: f32,
    rank: RankWeights,
    /// Absolute floor on the merged hybrid score: hits below it are
    /// embedding noise. Calibrated against the CPU-hash fallback (n-gram
    /// collisions for unrelated texts land at ≈0.10–0.18 merged; real hits
    /// with a text component start at ≈0.30) — chitchat queries must return
    /// nothing. A bge-m3 deployment may want to raise it (its noise baseline
    /// sits higher). Env `SLC_SEARCH_MIN_SCORE` (default 0.20).
    min_score: f32,
    /// Relative floor: a hit must score at least `gap_ratio × top score`.
    /// When the top hit is strong, the weak tail is not worth prompt tokens
    /// even if it cleared the absolute floor. Env `SLC_SEARCH_MIN_GAP`
    /// (default 0.6).
    gap_ratio: f32,
    /// One-shot embedding refresh per process: when the embedding model
    /// changes (e.g. hash → candle), stale records are re-generated lazily
    /// on the first search.
    refreshed: std::sync::Mutex<bool>,
}

impl SearchService {
    pub fn new(
        store: std::sync::Arc<dyn StorageBackend>,
        llm: std::sync::Arc<dyn LlmClient>,
        semantic_weight: f32,
        text_weight: f32,
    ) -> Self {
        SearchService {
            store,
            llm,
            semantic_weight,
            text_weight,
            rank: RankWeights::default(),
            min_score: env_f("SLC_SEARCH_MIN_SCORE", 0.20),
            gap_ratio: env_f("SLC_SEARCH_MIN_GAP", 0.6),
            refreshed: std::sync::Mutex::new(false),
        }
    }

    pub async fn search(
        &self,
        query: &str,
        category: Option<DocumentCategory>,
        tags: Option<Vec<String>>,
        limit: usize,
        seat_id: Option<&str>,
    ) -> SlcResult<Vec<SearchHit>> {
        let mut filter = DocFilter::default();
        filter.category = category;
        filter.tags_any = tags.unwrap_or_default();
        if let Some(seat) = seat_id {
            filter.visible_to = Some(seat.to_string());
        }

        // Semantic is best-effort: if the embedding endpoint is down, fall
        // back to text-only (legacy behavior) instead of failing the search.
        let sem = match self.semantic_search(query, &filter, limit * 2).await {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!("semantic search unavailable ({e}) — text-only");
                Vec::new()
            }
        };
        let txt = self.text_search(query, &filter, limit * 2).await?;

        // Merge: semantic seeded with semantic_weight, text adds text_weight
        // (max-text normalized), sum when present in both.
        let max_text = txt.iter().map(|h| h.score).fold(0.0f32, f32::max).max(1e-6);
        let mut merged: HashMap<String, SearchHit> = HashMap::new();
        for mut h in sem {
            h.score *= self.semantic_weight;
            merged.entry(h.document.document_id.clone()).or_insert(h);
        }
        for mut h in txt {
            let w = (h.score / max_text) * self.text_weight;
            match merged.get_mut(&h.document.document_id) {
                Some(existing) => existing.score += w,
                None => {
                    h.score = w;
                    merged.insert(h.document.document_id.clone(), h);
                }
            }
        }
        let mut hits: Vec<SearchHit> = merged.into_values().collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Relevance gate: drop embedding noise BEFORE the reranker blends
        // importance/recency in — an irrelevant document must never reach
        // the RAG block. Two guards: an absolute floor (kills the hash
        // fallback's near-zero noise) and a relative gap to the top hit
        // (kills the weak tail when the top hit is strong).
        let top = hits
            .iter()
            .map(|h| h.score)
            .fold(f32::NEG_INFINITY, f32::max);
        if top < self.min_score {
            return Ok(Vec::new());
        }
        let floor = top * self.gap_ratio;
        hits.retain(|h| h.score >= floor);
        Ok(rerank_inverted(hits, &self.rank, limit))
    }

    /// Cosine over embedding chunks; best score per document.
    async fn semantic_search(
        &self,
        query: &str,
        filter: &DocFilter,
        limit: usize,
    ) -> SlcResult<Vec<SearchHit>> {
        let qv = self
            .llm
            .generate_embedding_kind(query, crate::llm::EmbeddingKind::Query)
            .await?;
        // One-shot migration: re-embed documents whose records were produced
        // by a different model (dimension mismatch), so switching the
        // embedding backend doesn't leave the KB unsearchable.
        self.refresh_stale_embeddings(qv.len(), filter).await;

        let mut by_doc: HashMap<String, f32> = HashMap::new();
        for scope in [EmbeddingScope::Public, EmbeddingScope::Private] {
            let seat = if scope == EmbeddingScope::Private {
                filter.visible_to.clone()
            } else {
                None
            };
            let records = self.store.all_embeddings(scope, seat.as_deref()).await?;
            for r in records {
                // Records from another embedding model are incomparable.
                if r.embedding_dimension != qv.len() {
                    continue;
                }
                let s = cosine_similarity(&qv, &r.embedding);
                let e = by_doc.entry(r.document_id.clone()).or_insert(0.0);
                if s > *e {
                    *e = s;
                }
            }
        }
        let mut scored: Vec<(String, f32)> = by_doc.into_iter().collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        let mut out = Vec::new();
        for (id, score) in scored {
            if let Some(doc) = self.store.kb_get(&id).await? {
                if filter_matches_doc(filter, &doc) {
                    out.push(SearchHit {
                        document: doc,
                        score,
                        rank_score: score,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Re-embed documents whose embedding records came from another model
    /// (dimension mismatch). Best-effort and one-shot per process: failures
    /// (model still downloading, transient errors) just skip that document —
    /// the search proceeds with whatever is compatible.
    async fn refresh_stale_embeddings(&self, dim: usize, filter: &DocFilter) {
        {
            let mut done = self.refreshed.lock().unwrap();
            if *done {
                return;
            }
            *done = true;
        }
        let mut stale: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for scope in [EmbeddingScope::Public, EmbeddingScope::Private] {
            let seat = if scope == EmbeddingScope::Private {
                filter.visible_to.clone()
            } else {
                None
            };
            match self.store.all_embeddings(scope, seat.as_deref()).await {
                Ok(records) => {
                    for r in records {
                        if r.embedding_dimension != dim && seen.insert(r.document_id.clone()) {
                            stale.push(r.document_id);
                        }
                    }
                }
                Err(e) => tracing::debug!("refresh: all_embeddings failed: {e}"),
            }
        }
        if stale.is_empty() {
            return;
        }
        tracing::info!(
            count = stale.len(),
            "re-embedding documents from a previous embedding model"
        );
        // Batch rebuild (one forward per chunk).
        let mut docs = Vec::with_capacity(stale.len());
        for id in stale {
            if let Ok(Some(doc)) = self.store.kb_get(&id).await {
                docs.push(doc);
            }
        }
        let done = crate::reembed_documents(self.store.as_ref(), self.llm.as_ref(), &docs).await;
        tracing::info!(
            reembedded = done,
            total = docs.len(),
            "embedding refresh finished"
        );
    }

    /// BM25 over tokenized KB content (candidate set = filtered docs).
    async fn text_search(
        &self,
        query: &str,
        filter: &DocFilter,
        limit: usize,
    ) -> SlcResult<Vec<SearchHit>> {
        let q_terms = tokenize(query);
        if q_terms.is_empty() {
            return Ok(Vec::new());
        }
        // Oversample candidates: text search alone can't know the final
        // ranking, so pull limit*3 and score in Rust.
        let docs = self
            .store
            .kb_find(filter, &DocSort::by_updated(SortDir::Desc), limit * 3)
            .await?;
        let total = docs.len().max(1);
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut tokenized: Vec<(Document, Vec<String>)> = Vec::with_capacity(docs.len());
        for d in docs {
            let toks = tokenize(&d.content);
            for t in term_freq(&toks).keys() {
                *df.entry(t.clone()).or_insert(0) += 1;
            }
            tokenized.push((d, toks));
        }
        let mut hits: Vec<SearchHit> = tokenized
            .into_iter()
            .map(|(d, toks)| {
                let s = bm25(&q_terms, &toks, &df, total);
                SearchHit {
                    document: d,
                    score: s,
                    rank_score: s,
                }
            })
            .filter(|h| h.score > 0.0)
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

fn filter_matches_doc(f: &DocFilter, doc: &Document) -> bool {
    if let Some(cat) = f.category {
        if doc.category != cat {
            return false;
        }
    }
    if let Some(vis) = &f.visible_to {
        if let Some(owner) = &doc.seat_id {
            if owner != vis {
                return false;
            }
        }
    }
    if !f.tags_any.is_empty() && !f.tags_any.iter().any(|t| doc.tags.contains(t)) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockLlm;
    use crate::storage::sqlite::SqliteStore;

    fn sample_docs() -> Vec<Document> {
        let mut d1 = Document::new(
            "rust-patterns",
            DocumentCategory::Documentation,
            "Rust ownership and borrowing patterns for async pipelines",
            Default::default(),
            vec!["memory:semantic".into()],
            None,
        );
        let mut d2 = Document::new(
            "vassista-plan",
            DocumentCategory::Project,
            "Vassista voice assistant roadmap: STT, TTS, memory",
            Default::default(),
            vec!["priority:high".into()],
            None,
        );
        d2.tags.push("memory:semantic".into());
        let d3 = Document::new(
            "grocery-list",
            DocumentCategory::Custom,
            "milk, eggs, bread, tomatoes",
            Default::default(),
            vec![],
            None,
        );
        d1.tags.push("memory:working".into());
        vec![d1, d2, d3]
    }

    #[tokio::test]
    async fn cosine_and_bm25_sanity() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert!((cosine_similarity(&[1.0, 2.0], &[2.0, 4.0]) - 1.0).abs() < 1e-5);
        assert_eq!(
            cosine_similarity(&[1.0], &[1.0, 2.0]),
            0.0,
            "length mismatch → 0"
        );

        let df: HashMap<String, usize> =
            [("rust".into(), 1), ("voice".into(), 1), ("milk".into(), 1)]
                .into_iter()
                .collect();
        let docs = sample_docs();
        let scores: Vec<f32> = docs
            .iter()
            .map(|d| bm25(&tokenize("rust memory"), &tokenize(&d.content), &df, 3))
            .collect();
        assert!(
            scores[0] > scores[2],
            "rust doc should outrank grocery list"
        );
    }

    #[tokio::test]
    async fn hybrid_search_ranks_kb() {
        let store: std::sync::Arc<dyn crate::storage::StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        for d in sample_docs() {
            store.kb_insert(&d).await.unwrap();
        }
        let llm: std::sync::Arc<dyn crate::llm::LlmClient> =
            std::sync::Arc::new(MockLlm::default());
        let svc = SearchService::new(store, llm, 0.7, 0.3);
        let hits = svc
            .search("voice assistant memory", None, None, 5, None)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "expected hits");
        // KB search must never return episodic docs (none inserted here, but
        // the store split guarantees it structurally).
        assert!(hits.iter().all(|h| h.document.category.is_kb()));
        let ids: Vec<&str> = hits
            .iter()
            .map(|h| h.document.document_id.as_str())
            .collect();
        assert!(
            ids.contains(&"vassista-plan"),
            "project doc should be found: {ids:?}"
        );
    }

    #[tokio::test]
    async fn reranker_importance_recency() {
        let hits = sample_docs()
            .into_iter()
            .enumerate()
            .map(|(i, d)| SearchHit {
                document: d,
                score: 0.5 - i as f32 * 0.1,
                rank_score: 0.0,
            })
            .collect();
        let ranked = rerank_inverted(hits, &RankWeights::default(), 3);
        assert_eq!(ranked.len(), 3);
        // vassista-plan has priority:high → importance 0.75 beats others at
        // equal-ish relevance; it must end up first or second, not last.
        let pos = ranked
            .iter()
            .position(|h| h.document.document_id == "vassista-plan")
            .unwrap();
        assert!(
            pos <= 1,
            "high-importance doc should rank near the top: {pos}"
        );
    }

    #[tokio::test]
    async fn stale_embeddings_are_filtered_and_refreshed() {
        let store: std::sync::Arc<dyn crate::storage::StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        for d in sample_docs() {
            store.kb_insert(&d).await.unwrap();
        }
        // Records from the "old model" (dim=2) — like after switching embedders.
        for id in ["rust-patterns", "vassista-plan", "grocery-list"] {
            store
                .insert_embeddings(&[crate::model::EmbeddingRecord {
                    document_id: (*id).into(),
                    chunk_index: 0,
                    chunk_total: 1,
                    embedding: vec![0.1, 0.2],
                    embedding_model: "old-model".into(),
                    embedding_dimension: 2,
                    generated_at: chrono::Utc::now(),
                    scope: crate::model::EmbeddingScope::Public,
                    seat_id: None,
                }])
                .await
                .unwrap();
        }
        let llm: std::sync::Arc<dyn crate::llm::LlmClient> =
            std::sync::Arc::new(MockLlm::default()); // dim=512
        let svc = SearchService::new(store.clone(), llm, 0.7, 0.3);
        let hits = svc
            .search("voice assistant memory", None, None, 5, None)
            .await
            .unwrap();
        assert!(
            !hits.is_empty(),
            "search must not break on stale embeddings"
        );
        // One-shot refresh re-embedded everything under the current model.
        for r in store
            .all_embeddings(crate::model::EmbeddingScope::Public, None)
            .await
            .unwrap()
        {
            assert_eq!(
                r.embedding_dimension, 512,
                "stale embedding must be re-embedded: {}",
                r.document_id
            );
        }
    }
}

#[cfg(test)]
mod relevance_gate {
    use super::*;
    use crate::llm::MockLlm;
    use crate::storage::sqlite::SqliteStore;

    /// One KB doc per case with REAL embedding records (as add_document
    /// would create them). The gate must keep the text/semantic hit and
    /// drop everything else.
    async fn seeded(query_and_docs: &[(&str, &str)]) -> SearchService {
        let store: std::sync::Arc<dyn crate::storage::StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        let llm: std::sync::Arc<dyn crate::llm::LlmClient> =
            std::sync::Arc::new(MockLlm::default());
        for (id, content) in query_and_docs {
            let doc = Document::new(
                *id,
                DocumentCategory::Documentation,
                *content,
                Default::default(),
                vec![],
                None,
            );
            store.kb_insert(&doc).await.unwrap();
            let emb = llm.generate_embedding(content).await.unwrap();
            let dim = emb.len();
            store
                .insert_embeddings(&[crate::model::EmbeddingRecord {
                    document_id: (*id).into(),
                    chunk_index: 0,
                    chunk_total: 1,
                    embedding: emb,
                    embedding_model: llm.embedding_model_name(),
                    embedding_dimension: dim,
                    generated_at: chrono::Utc::now(),
                    scope: crate::model::EmbeddingScope::Public,
                    seat_id: None,
                }])
                .await
                .unwrap();
        }
        SearchService::new(store, llm, 0.7, 0.3)
    }

    #[tokio::test]
    async fn irrelevant_query_returns_nothing() {
        let svc = seeded(&[
            ("fact_voice", "Меня зовут Дмитрий, обращайся ко мне по имени"),
            ("note_coffee", "Пользователь предпочитает чёрный кофе без сахара"),
        ])
        .await;
        // No token/n-gram overlap with either doc — pure embedding noise
        // must not survive the gate.
        for q in ["привет", "рецепт пасты карбонара"] {
            let hits = svc.search(q, None, None, 5, None).await.unwrap();
            assert!(hits.is_empty(), "Q={q:?} leaked: {:?}", hits.iter().map(|h| (&h.document.document_id, h.score)).collect::<Vec<_>>());
        }
    }

    #[tokio::test]
    async fn strong_hit_kept_weak_tail_dropped() {
        let svc = seeded(&[
            ("fact_voice", "Меня зовут Дмитрий, обращайся ко мне по имени"),
            ("note_coffee", "Пользователь предпочитает чёрный кофе без сахара"),
        ])
        .await;
        // Text overlap with the query plus shared n-grams: the fact doc
        // is clearly relevant; the coffee note shares no tokens and its
        // hash-noise must fall below the relative gap.
        let hits = svc.search("как меня зовут", None, None, 5, None).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|h| h.document.document_id.as_str()).collect();
        assert!(ids.contains(&"fact_voice"), "relevant doc must survive: {ids:?}");
        assert!(!ids.contains(&"note_coffee"), "irrelevant tail must be gated out: {ids:?}");
    }
}
