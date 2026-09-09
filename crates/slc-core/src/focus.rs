//! FocusManager — CRUD and queries for focus items.
//!
//! Legacy equivalent: `src/focus/manager.py` + `models.py`. A focus is
//! something the user is concentrating on. Focuses decay over time; the ones
//! whose decay score drops below threshold are auto-archived. Hard cap
//! `MAX_FOCUSES` (7), soft `MAX_FOCUSES_SOFT` (5).
//!
//! Focuses are stored as free-form JSON records in the storage backend
//! (`collection = "focuses"`, keyed by `focus_id`), like seats/timers — they
//! are operational state, not RAG-eligible documents.

use crate::error::{SlcError, SlcResult};
use crate::proactivity::{MindType, mind_matches, normalize_write_mind_type};
use crate::storage::StorageBackend;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const COLLECTION: &str = "focuses";

/// Hard cap on active focuses (any higher and `add` fails).
pub const MAX_FOCUSES: usize = 7;
/// Soft cap — not strictly enforced, informational for UX.
pub const MAX_FOCUSES_SOFT: usize = 5;
/// Exponential-decay constant (per hour).
pub const DECAY_LAMBDA: f64 = 0.02;
/// Below this decay score a focus is considered spent / auto-archived.
pub const DECAY_THRESHOLD: f64 = 0.1;

/// A single focus — something the user is concentrating on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusItem {
    pub focus_id: String,
    pub seat_id: String,
    pub mind_type: MindType,
    pub title: String,
    pub description: String,
    /// 1..=10, higher = more important.
    pub priority: i64,
    pub depends_on: Vec<String>,
    pub reminder_count: i64,
    pub created_at: DateTime<Utc>,
    pub archived: bool,
}

impl FocusItem {
    /// Exponential decay: `exp(-λ * age_h / (1 + reminder_count))`.
    pub fn decay_score(&self, decay_lambda: f64) -> f64 {
        let age_hours = (Utc::now() - self.created_at).num_seconds() as f64 / 3600.0;
        (-decay_lambda * age_hours / (1.0 + self.reminder_count as f64)).exp()
    }
}

/// CRUD + query manager over the `focuses` records collection.
#[derive(Clone)]
pub struct FocusManager<S: StorageBackend> {
    store: S,
}

impl<S: StorageBackend> FocusManager<S> {
    pub fn new(store: S) -> Self {
        FocusManager { store }
    }

    pub async fn add(
        &self,
        seat_id: &str,
        title: &str,
        description: &str,
        priority: i64,
        depends_on: &[String],
        mind_type: Option<&str>,
    ) -> SlcResult<FocusItem> {
        let mt = normalize_write_mind_type(mind_type).map_err(SlcError::InvalidInput)?;
        let active = self.get_active(seat_id, Some(mt)).await?;
        if active.len() >= MAX_FOCUSES {
            return Err(SlcError::Limit(format!(
                "Hard limit reached ({MAX_FOCUSES} focuses). Remove some before adding new ones."
            )));
        }

        let focus_id = crate::model::unique_id("foc");
        let deps: Vec<String> = depends_on.to_vec();
        if !deps.is_empty() {
            self.check_cycle(seat_id, &focus_id, &deps).await?;
        }

        let item = FocusItem {
            focus_id,
            seat_id: seat_id.into(),
            mind_type: mt,
            title: title.into(),
            description: description.into(),
            priority: priority.clamp(1, 10),
            depends_on: deps,
            reminder_count: 0,
            created_at: Utc::now(),
            archived: false,
        };
        self.put(&item).await?;
        Ok(item)
    }

    pub async fn remove(&self, focus_id: &str, seat_id: Option<&str>) -> SlcResult<bool> {
        let Some(item) = self.get(focus_id, seat_id).await? else {
            return Ok(false);
        };
        self.store.delete_record(COLLECTION, focus_id).await?;
        // Clean up references to the removed focus in other focuses.
        let mut fix = Vec::new();
        for (_, val) in self.store.list_records(COLLECTION).await? {
            if let Ok(mut other) = serde_json::from_value::<FocusItem>(val) {
                if other.seat_id == item.seat_id && other.depends_on.iter().any(|d| d == focus_id) {
                    other.depends_on.retain(|d| d != focus_id);
                    fix.push(other);
                }
            }
        }
        for other in fix {
            self.put(&other).await?;
        }
        Ok(true)
    }

    pub async fn update(
        &self,
        focus_id: &str,
        title: Option<&str>,
        description: Option<&str>,
        priority: Option<i64>,
        depends_on: Option<&[String]>,
        seat_id: Option<&str>,
    ) -> SlcResult<bool> {
        let Some(mut item) = self.get(focus_id, seat_id).await? else {
            return Ok(false);
        };
        let mut changed = false;
        if let Some(t) = title {
            item.title = t.into();
            changed = true;
        }
        if let Some(d) = description {
            item.description = d.into();
            changed = true;
        }
        if let Some(p) = priority {
            item.priority = p.clamp(1, 10);
            changed = true;
        }
        if let Some(deps) = depends_on {
            self.check_cycle(&item.seat_id, focus_id, deps).await?;
            item.depends_on = deps.to_vec();
            changed = true;
        }
        if !changed {
            return Ok(false);
        }
        self.put(&item).await?;
        Ok(true)
    }

    /// Manually archive/unarchive a focus (owner — seat_id).
    pub async fn set_archived(
        &self,
        focus_id: &str,
        seat_id: &str,
        archived: bool,
    ) -> SlcResult<bool> {
        let Some(mut item) = self.get(focus_id, Some(seat_id)).await? else {
            return Ok(false);
        };
        if item.archived == archived {
            return Ok(true);
        }
        item.archived = archived;
        self.put(&item).await?;
        Ok(true)
    }

    pub async fn get(&self, focus_id: &str, seat_id: Option<&str>) -> SlcResult<Option<FocusItem>> {
        let Some(val) = self.store.get_record(COLLECTION, focus_id).await? else {
            return Ok(None);
        };
        let item: FocusItem = serde_json::from_value(val)?;
        if let Some(s) = seat_id {
            if item.seat_id != s {
                return Ok(None);
            }
        }
        Ok(Some(item))
    }

    /// Non-archived focuses with decay above threshold, sorted by priority
    /// (highest first). `mind_type: None` → no scope filter.
    pub async fn get_active(
        &self,
        seat_id: &str,
        mind_type: Option<MindType>,
    ) -> SlcResult<Vec<FocusItem>> {
        let mut items = self.list_raw(seat_id, false).await?;
        items.retain(|i| {
            mind_matches(i.mind_type, mind_type) && i.decay_score(DECAY_LAMBDA) >= DECAY_THRESHOLD
        });
        items.sort_by(|a, b| b.priority.cmp(&a.priority));
        Ok(items)
    }

    /// Active focuses whose dependencies are all resolved.
    pub async fn get_unblocked(
        &self,
        seat_id: &str,
        mind_type: Option<MindType>,
    ) -> SlcResult<Vec<FocusItem>> {
        let active = self.get_active(seat_id, mind_type).await?;
        let active_ids: std::collections::HashSet<String> =
            active.iter().map(|f| f.focus_id.clone()).collect();
        Ok(active
            .into_iter()
            .filter(|f| f.depends_on.iter().all(|d| !active_ids.contains(d)))
            .collect())
    }

    pub async fn list_archived(
        &self,
        seat_id: &str,
        mind_type: Option<MindType>,
    ) -> SlcResult<Vec<FocusItem>> {
        let items = self.list_raw(seat_id, true).await?;
        Ok(items
            .into_iter()
            .filter(|i| mind_matches(i.mind_type, mind_type))
            .collect())
    }

    /// Archive focuses whose decay score dropped below threshold. Returns the
    /// number archived.
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

    pub async fn increment_reminder(
        &self,
        focus_id: &str,
        seat_id: Option<&str>,
    ) -> SlcResult<bool> {
        let Some(mut item) = self.get(focus_id, seat_id).await? else {
            return Ok(false);
        };
        item.reminder_count += 1;
        self.put(&item).await?;
        Ok(true)
    }

    pub async fn count_active(
        &self,
        seat_id: &str,
        mind_type: Option<MindType>,
    ) -> SlcResult<usize> {
        Ok(self.get_active(seat_id, mind_type).await?.len())
    }

    /// DFS cycle detection in the dependency graph.
    async fn check_cycle(&self, seat_id: &str, focus_id: &str, deps: &[String]) -> SlcResult<()> {
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut stack: Vec<String> = deps.to_vec();
        while let Some(current) = stack.pop() {
            if current == focus_id {
                return Err(SlcError::InvalidInput(format!(
                    "Cycle detected: {focus_id} → … → {focus_id}"
                )));
            }
            if visited.contains(&current) {
                continue;
            }
            visited.insert(current.clone());
            let Some(item) = self.get(&current, Some(seat_id)).await? else {
                return Err(SlcError::InvalidInput(
                    "Focus dependency not found for Seat".into(),
                ));
            };
            stack.extend(item.depends_on);
        }
        Ok(())
    }

    async fn put(&self, item: &FocusItem) -> SlcResult<()> {
        self.store
            .put_record(COLLECTION, &item.focus_id, &serde_json::to_value(item)?)
            .await
    }

    async fn list_raw(&self, seat_id: &str, archived: bool) -> SlcResult<Vec<FocusItem>> {
        let mut out = Vec::new();
        for (_, val) in self.store.list_records(COLLECTION).await? {
            if let Ok(item) = serde_json::from_value::<FocusItem>(val) {
                if item.seat_id == seat_id && item.archived == archived {
                    out.push(item);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    async fn manager() -> (
        FocusManager<std::sync::Arc<dyn StorageBackend>>,
        std::sync::Arc<dyn StorageBackend>,
    ) {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (FocusManager::new(store.clone()), store)
    }

    #[tokio::test]
    async fn add_get_active_cycle_and_archive() {
        let (m, _) = manager().await;
        let a = m
            .add("seat1", "Ship MVP", "desc", 8, &[], None)
            .await
            .unwrap();
        assert!(a.focus_id.starts_with("foc_"));
        assert_eq!(a.priority, 8);

        // B depends on A (which exists) → allowed, no cycle
        let b = m
            .add("seat1", "B", "", 5, &[a.focus_id.clone()], None)
            .await
            .unwrap();
        let unblocked = m.get_unblocked("seat1", None).await.unwrap();
        // A is unblocked, B depends on A (which is active) → B blocked
        let ids: Vec<_> = unblocked.iter().map(|f| f.focus_id.clone()).collect();
        assert!(ids.contains(&a.focus_id));
        assert!(!ids.contains(&b.focus_id));

        // a cycle: C depends on A, then update A to depend on C
        let c = m
            .add("seat1", "C", "", 5, &[a.focus_id.clone()], None)
            .await
            .unwrap();
        let err = m
            .update(
                &a.focus_id,
                None,
                None,
                None,
                Some(&[c.focus_id.clone()]),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Cycle detected"));

        // hard limit (already have a, b, c = 3)
        for i in 0..4 {
            m.add("seat1", &format!("F{i}"), "", 3, &[], None)
                .await
                .unwrap();
        }
        let err = m
            .add("seat1", "overflow", "", 1, &[], None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Hard limit"));
    }

    #[tokio::test]
    async fn decay_and_auto_archive() {
        let (m, _) = manager().await;
        let item = m.add("seat2", "Old", "", 5, &[], None).await.unwrap();
        // simulate an old item
        let mut old = item.clone();
        old.created_at = Utc::now() - chrono::Duration::days(30);
        old.reminder_count = 0;
        m.put(&old).await.unwrap();
        assert!(old.decay_score(DECAY_LAMBDA) < DECAY_THRESHOLD);

        let archived = m.auto_archive("seat2").await.unwrap();
        assert_eq!(archived, 1);
        assert!(m.get_active("seat2", None).await.unwrap().is_empty());
        assert_eq!(m.list_archived("seat2", None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_and_remove() {
        let (m, _) = manager().await;
        let item = m.add("seat3", "X", "old", 4, &[], None).await.unwrap();
        assert!(
            m.update(&item.focus_id, Some("Y"), Some("new"), Some(9), None, None)
                .await
                .unwrap()
        );
        let got = m.get(&item.focus_id, None).await.unwrap().unwrap();
        assert_eq!(got.title, "Y");
        assert_eq!(got.description, "new");
        assert_eq!(got.priority, 9);

        assert!(m.remove(&item.focus_id, None).await.unwrap());
        assert!(m.get(&item.focus_id, None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn seat_scoping_and_reminder() {
        let (m, _) = manager().await;
        let item = m.add("seat_a", "Focus A", "", 5, &[], None).await.unwrap();
        assert!(
            m.get(&item.focus_id, Some("seat_b"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            m.increment_reminder(&item.focus_id, Some("seat_a"))
                .await
                .unwrap()
        );
        let got = m.get(&item.focus_id, None).await.unwrap().unwrap();
        assert_eq!(got.reminder_count, 1);
    }
}
