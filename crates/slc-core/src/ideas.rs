//! IdeaPool — CRUD, weighted sampling, and auto-activation of ideas.
//!
//! Legacy equivalent: `src/ideas/pool.py` + `models.py`. An idea is a
//! captured insight that can be surfaced proactively. Ideas decay over time;
//! selection is weighted toward fresh, rarely-shown ideas; ideas whose
//! embedding is similar to the current context are auto-activated.
//!
//! Stored as free-form JSON records (`collection = "ideas"`, keyed by
//! `idea_id`) — operational state, not RAG-eligible documents.

use crate::error::{SlcError, SlcResult};
use crate::proactivity::{mind_matches, normalize_write_mind_type, MindType};
use crate::search::cosine_similarity;
use crate::storage::StorageBackend;
use chrono::{DateTime, Utc};
use rand::seq::SliceRandom;
use rand::thread_rng;
use serde::{Deserialize, Serialize};

pub const COLLECTION: &str = "ideas";

/// Hard cap on active ideas.
pub const MAX_IDEAS: usize = 50;
/// Soft cap — informational.
pub const MAX_IDEAS_SOFT: usize = 30;
pub const DECAY_LAMBDA: f64 = 0.02;
pub const DECAY_THRESHOLD: f64 = 0.1;
/// Cosine similarity above which an idea is auto-activated by context.
pub const ACTIVATION_THRESHOLD: f64 = 0.75;

/// A single idea stored in the weighted pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdeaItem {
    pub idea_id: String,
    pub seat_id: String,
    pub mind_type: MindType,
    pub content: String,
    /// `manual` | `reflection` | `collective`.
    pub source: String,
    pub embedding: Option<Vec<f32>>,
    pub activation_count: i64,
    pub reminded_count: i64,
    pub last_reminded_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub archived: bool,
}

impl IdeaItem {
    pub fn decay_score(&self, decay_lambda: f64) -> f64 {
        let age_hours = (Utc::now() - self.created_at).num_seconds() as f64 / 3600.0;
        (-decay_lambda * age_hours / (1.0 + self.reminded_count as f64)).exp()
    }

    /// Weight for random selection: favours fresh, rarely-shown ideas.
    pub fn weight(&self, decay_lambda: f64) -> f64 {
        let ds = self.decay_score(decay_lambda);
        ds / (1.0 + self.reminded_count as f64)
    }
}

/// CRUD + weighted-sampling manager over the `ideas` records collection.
#[derive(Clone)]
pub struct IdeaPool<S: StorageBackend> {
    store: S,
}

impl<S: StorageBackend> IdeaPool<S> {
    pub fn new(store: S) -> Self {
        IdeaPool { store }
    }

    pub async fn add(
        &self,
        seat_id: &str,
        content: &str,
        source: &str,
        embedding: Option<Vec<f32>>,
        mind_type: Option<&str>,
    ) -> SlcResult<IdeaItem> {
        let mt = normalize_write_mind_type(mind_type).map_err(SlcError::InvalidInput)?;
        let active_count = self.list_raw(seat_id, false).await?.len();
        if active_count >= MAX_IDEAS {
            return Err(SlcError::Limit(format!("Hard limit reached ({MAX_IDEAS} ideas).")));
        }
        let item = IdeaItem {
            idea_id: crate::model::unique_id("idea"),
            seat_id: seat_id.into(),
            mind_type: mt,
            content: content.into(),
            source: source.into(),
            embedding,
            activation_count: 0,
            reminded_count: 0,
            last_reminded_at: None,
            created_at: Utc::now(),
            archived: false,
        };
        self.put(&item).await?;
        Ok(item)
    }

    pub async fn remove(&self, idea_id: &str, seat_id: Option<&str>) -> SlcResult<bool> {
        if let Some(s) = seat_id {
            if let Some(item) = self.get(idea_id, None).await? {
                if item.seat_id != s {
                    return Ok(false);
                }
            }
        }
        self.store.delete_record(COLLECTION, idea_id).await
    }

    pub async fn get(&self, idea_id: &str, seat_id: Option<&str>) -> SlcResult<Option<IdeaItem>> {
        let Some(val) = self.store.get_record(COLLECTION, idea_id).await? else { return Ok(None) };
        let item: IdeaItem = serde_json::from_value(val)?;
        if let Some(s) = seat_id {
            if item.seat_id != s {
                return Ok(None);
            }
        }
        Ok(Some(item))
    }

    /// Active, above-threshold ideas, scoped by `mind_type`.
    pub async fn list_active(&self, seat_id: &str, limit: usize, mind_type: Option<MindType>) -> SlcResult<Vec<IdeaItem>> {
        let items = self.list_raw(seat_id, false).await?;
        Ok(items
            .into_iter()
            .filter(|i| mind_matches(i.mind_type, mind_type) && i.decay_score(DECAY_LAMBDA) >= DECAY_THRESHOLD)
            .take(limit)
            .collect())
    }

    pub async fn list_archived(&self, seat_id: &str, mind_type: Option<MindType>) -> SlcResult<Vec<IdeaItem>> {
        let items = self.list_raw(seat_id, true).await?;
        Ok(items.into_iter().filter(|i| mind_matches(i.mind_type, mind_type)).collect())
    }

    /// Select an idea by weighted probability; bumps its reminded count.
    pub async fn get_weighted_random(&self, seat_id: &str, mind_type: Option<MindType>) -> SlcResult<Option<IdeaItem>> {
        let active = self.list_active(seat_id, MAX_IDEAS, mind_type).await?;
        let chosen = pick_weighted(&active)?;
        let Some(chosen) = chosen else { return Ok(None) };
        let mut item = chosen.clone();
        item.reminded_count += 1;
        item.last_reminded_at = Some(Utc::now());
        self.put(&item).await?;
        Ok(Some(item))
    }

    /// Auto-activate ideas whose embedding is similar to the context.
    /// Returns the list of activated idea ids.
    pub async fn check_activations(&self, seat_id: &str, context_embedding: &[f32]) -> SlcResult<Vec<String>> {
        let items = self.list_raw(seat_id, false).await?;
        let mut activated = Vec::new();
        for mut item in items {
            let Some(emb) = &item.embedding else { continue };
            if cosine_similarity(context_embedding, emb) >= ACTIVATION_THRESHOLD as f32 {
                item.activation_count += 1;
                self.put(&item).await?;
                activated.push(item.idea_id.clone());
            }
        }
        Ok(activated)
    }

    /// Archive ideas whose decay score dropped below threshold.
    pub async fn auto_archive(&self, seat_id: &str) -> SlcResult<usize> {
        let items = self.list_raw(seat_id, false).await?;
        let mut archived = 0;
        for mut item in items {
            if item.decay_score(DECAY_LAMBDA) < DECAY_THRESHOLD {
                item.archived = true;
                self.put(&item).await?;
                archived += 1;
            }
        }
        Ok(archived)
    }

    pub async fn count_active(&self, seat_id: &str, mind_type: Option<MindType>) -> SlcResult<usize> {
        Ok(self.list_active(seat_id, MAX_IDEAS, mind_type).await?.len())
    }

    async fn put(&self, item: &IdeaItem) -> SlcResult<()> {
        self.store
            .put_record(COLLECTION, &item.idea_id, &serde_json::to_value(item)?)
            .await
    }

    async fn list_raw(&self, seat_id: &str, archived: bool) -> SlcResult<Vec<IdeaItem>> {
        let mut out = Vec::new();
        for (_, val) in self.store.list_records(COLLECTION).await? {
            if let Ok(item) = serde_json::from_value::<IdeaItem>(val) {
                if item.seat_id == seat_id && item.archived == archived {
                    out.push(item);
                }
            }
        }
        Ok(out)
    }
}

/// Weighted-random pick (pure: no async, no non-`Send` RNG crossing awaits).
fn pick_weighted(active: &[IdeaItem]) -> SlcResult<Option<&IdeaItem>> {
    if active.is_empty() {
        return Ok(None);
    }
    let mut rng = thread_rng();
    let chosen = active
        .choose_weighted(&mut rng, |i| i.weight(DECAY_LAMBDA).max(1e-9))
        .map_err(|e| SlcError::Storage(e.to_string()))?;
    Ok(Some(chosen))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    fn pool() -> (IdeaPool<std::sync::Arc<dyn StorageBackend>>, std::sync::Arc<dyn StorageBackend>) {
        let store: std::sync::Arc<dyn StorageBackend> = std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (IdeaPool::new(store.clone()), store)
    }

    #[tokio::test]
    async fn add_list_remove_and_limit() {
        let (p, _) = pool();
        let item = p.add("seat_i", "capture more audio", "reflection", None, None).await.unwrap();
        assert!(item.idea_id.starts_with("idea_"));
        assert_eq!(item.source, "reflection");
        assert_eq!(p.list_active("seat_i", 50, None).await.unwrap().len(), 1);
        assert!(p.remove(&item.idea_id, None).await.unwrap());
        assert!(p.get(&item.idea_id, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn weighted_random_prefers_fresh() {
        let (p, _) = pool();
        // A fresh idea (high weight) vs an old, often-reminded one.
        let old = p.add("seat_w", "stale idea", "manual", None, None).await.unwrap();
        let mut stale = old.clone();
        stale.created_at = Utc::now() - chrono::Duration::days(10);
        stale.reminded_count = 50;
        p.put(&stale).await.unwrap();

        let fresh = p.add("seat_w", "brand new idea", "manual", None, None).await.unwrap();

        let mut picked_fresh = 0;
        for _ in 0..30 {
            let chosen = p.get_weighted_random("seat_w", None).await.unwrap().unwrap();
            if chosen.idea_id == fresh.idea_id {
                picked_fresh += 1;
            }
        }
        assert!(picked_fresh > 15, "fresh idea should dominate: got {picked_fresh}/30");
        // reminded_count bumped
        let got = p.get(&fresh.idea_id, None).await.unwrap().unwrap();
        assert!(got.reminded_count >= 1);
        assert!(got.last_reminded_at.is_some());
    }

    #[tokio::test]
    async fn activation_by_similarity() {
        let (p, _) = pool();
        // identical embeddings → cosine 1.0 ≥ 0.75 → activated
        let with_emb = p.add("seat_a", "audio latency", "manual", Some(vec![1.0, 0.0, 0.0]), None).await.unwrap();
        let no_emb = p.add("seat_a", "no embed", "manual", None, None).await.unwrap();
        let activated = p.check_activations("seat_a", &[1.0, 0.0, 0.0]).await.unwrap();
        assert!(activated.contains(&with_emb.idea_id));
        assert!(!activated.contains(&no_emb.idea_id));
        let got = p.get(&with_emb.idea_id, None).await.unwrap().unwrap();
        assert_eq!(got.activation_count, 1);
    }

    #[tokio::test]
    async fn auto_archive_stale() {
        let (p, _) = pool();
        let item = p.add("seat_z", "old idea", "manual", None, None).await.unwrap();
        let mut old = item.clone();
        old.created_at = Utc::now() - chrono::Duration::days(30);
        p.put(&old).await.unwrap();
        let archived = p.auto_archive("seat_z").await.unwrap();
        assert_eq!(archived, 1);
        assert!(p.list_active("seat_z", 50, None).await.unwrap().is_empty());
    }
}
