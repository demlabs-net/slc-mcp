//! NotificationQueue — record-backed queue for proactive notifications.
//!
//! Legacy equivalent: `src/notifications/queue.py`. Timers push notifications
//! here; the MCP tool wrapper pops pending ones and injects them into the
//! next tool response (polling fallback). The queue is stored as free-form
//! JSON records (`collection = "notifications"`, keyed by `notification_id`).
//!
//! Delivery is two-fold in the legacy:
//! 1. **Prompt signal** — if a SessionNotifier is attached, fire a
//!    prompts-changed signal so the client re-fetches prompts.
//! 2. **Polling fallback** — the MCP wrapper pops pending notifications.
//! The Rust port implements the polling fallback (the core, storage-backed
//! part); prompt-list signalling is deferred (MCP SSE is out of scope here).

use crate::error::SlcResult;
use crate::model::Notification;
use crate::storage::StorageBackend;
use chrono::{Duration, Utc};

pub const COLLECTION: &str = "notifications";

/// Auto-delete notifications older than this (legacy TTL).
pub const TTL_SECONDS: i64 = 86400;

/// Queue of notifications, backed by the records store.
#[derive(Clone)]
pub struct NotificationQueue<S: StorageBackend> {
    store: S,
}

impl<S: StorageBackend> NotificationQueue<S> {
    pub fn new(store: S) -> Self {
        NotificationQueue { store }
    }

    /// Push a pending notification; returns its id.
    pub async fn push(
        &self,
        seat_id: &str,
        source: &str,
        title: &str,
        body: &str,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> SlcResult<String> {
        let nid = crate::model::unique_id("ntf");
        let notif = Notification {
            notification_id: nid.clone(),
            seat_id: seat_id.into(),
            source: source.into(),
            title: title.into(),
            body: body.into(),
            status: "pending".into(),
            metadata,
            created_at: Utc::now(),
            delivered_at: None,
        };
        self.put(&notif).await?;
        Ok(nid)
    }

    /// Atomically fetch and mark up to `limit` pending notifications as
    /// delivered (oldest first).
    pub async fn pop_pending(&self, seat_id: &str, limit: usize) -> SlcResult<Vec<Notification>> {
        let mut pending = self.list(seat_id, Some("pending")).await?;
        pending.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let now = Utc::now();
        let mut out = Vec::new();
        for mut n in pending.into_iter().take(limit) {
            n.status = "delivered".into();
            n.delivered_at = Some(now);
            self.put(&n).await?;
            out.push(n);
        }
        Ok(out)
    }

    pub async fn count_pending(&self, seat_id: &str) -> SlcResult<usize> {
        Ok(self.list(seat_id, Some("pending")).await?.len())
    }

    /// Enumerate notifications for a seat, optionally filtered by status.
    pub async fn list(&self, seat_id: &str, status: Option<&str>) -> SlcResult<Vec<Notification>> {
        let mut out = Vec::new();
        for (_, val) in self.store.list_records(COLLECTION).await? {
            if let Ok(n) = serde_json::from_value::<Notification>(val) {
                if n.seat_id == seat_id && status.map(|s| n.status == s).unwrap_or(true) {
                    out.push(n);
                }
            }
        }
        Ok(out)
    }

    /// Delete delivered notifications older than `ttl_seconds` (24h).
    pub async fn cleanup(&self, seat_id: &str, ttl_seconds: i64) -> SlcResult<usize> {
        let cutoff = Utc::now() - Duration::seconds(ttl_seconds);
        let all = self.list(seat_id, None).await?;
        let mut removed = 0;
        for n in all {
            if n.status == "delivered" && n.created_at < cutoff {
                if self.store.delete_record(COLLECTION, &n.notification_id).await? {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    async fn put(&self, n: &Notification) -> SlcResult<()> {
        self.store
            .put_record(COLLECTION, &n.notification_id, &serde_json::to_value(n)?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;
    use serde_json::json;

    fn queue() -> (NotificationQueue<std::sync::Arc<dyn StorageBackend>>, std::sync::Arc<dyn StorageBackend>) {
        let store: std::sync::Arc<dyn StorageBackend> = std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (NotificationQueue::new(store.clone()), store)
    }

    #[tokio::test]
    async fn push_pop_and_count() {
        let (q, _) = queue();
        let id = q.push("seat_n", "REMINDER", "⏰ Reminder", "Do the thing", Default::default()).await.unwrap();
        assert!(id.starts_with("ntf_"));
        assert_eq!(q.count_pending("seat_n").await.unwrap(), 1);

        let popped = q.pop_pending("seat_n", 5).await.unwrap();
        assert_eq!(popped.len(), 1);
        assert_eq!(popped[0].status, "delivered");
        assert!(popped[0].delivered_at.is_some());
        assert_eq!(q.count_pending("seat_n").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn seat_isolation() {
        let (q, _) = queue();
        q.push("seat_a", "REMINDER", "t", "b", Default::default()).await.unwrap();
        q.push("seat_b", "REMINDER", "t", "b", Default::default()).await.unwrap();
        assert_eq!(q.count_pending("seat_a").await.unwrap(), 1);
        assert_eq!(q.count_pending("seat_b").await.unwrap(), 1);
        let popped = q.pop_pending("seat_a", 5).await.unwrap();
        assert_eq!(popped.len(), 1);
        assert_eq!(popped[0].seat_id, "seat_a");
        assert_eq!(q.count_pending("seat_b").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn metadata_roundtrip_and_cleanup() {
        let (q, _) = queue();
        let mut meta = serde_json::Map::new();
        meta.insert("reminder_id".into(), json!("rem_abc"));
        let id = q.push("seat_m", "REMINDER", "t", "b", meta.clone()).await.unwrap();
        let popped = q.pop_pending("seat_m", 5).await.unwrap();
        assert_eq!(popped[0].metadata.get("reminder_id").and_then(|v| v.as_str()), Some("rem_abc"));
        assert_eq!(id, popped[0].notification_id);
        assert_eq!(q.cleanup("seat_m", TTL_SECONDS).await.unwrap(), 0);
    }
}
