//! Multi-seat isolation — each client owns its documents and context.

use crate::error::SlcResult;
use crate::model::{Seat, SeatStatus, UsageStats, unique_id};
use crate::storage::StorageBackend;
use chrono::{Duration, Utc};
use serde_json::{json, Map, Value};

/// Seat lifecycle manager.
pub struct SeatManager<S: StorageBackend> {
    store: S,
    /// Seat TTL in seconds (0 = never expire).
    ttl: i64,
}

impl<S: StorageBackend> SeatManager<S> {
    pub fn new(store: S, ttl_seconds: i64) -> Self {
        SeatManager { store, ttl: ttl_seconds }
    }

    pub fn with_env(store: S) -> Self {
        let ttl = std::env::var("SLC_SEAT_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(86400);
        Self::new(store, ttl)
    }

    /// Create a seat; caller-supplied `seat_id` is kept as-is (≤128 chars,
    /// non-JWT) — otherwise auto-generated `slc_session_{…}`.
    pub async fn create_seat(&self, seat_id: Option<String>, name: Option<String>, metadata: Option<Map<String, Value>>) -> SlcResult<Seat> {
        let now = Utc::now();
        let generated = match &seat_id {
            Some(id) if !id.is_empty() && id.len() <= 128 && !id.starts_with("eyJ") => id.clone(),
            _ => {
                let ts = now.format("%Y%m%d_%H%M%S");
                format!("slc_session_{ts}_{}", &unique_id("")[1..])
            }
        };
        let seat = Seat {
            seat_id: generated,
            name: name.unwrap_or_else(|| "unnamed".into()),
            status: SeatStatus::Active,
            created_at: now,
            last_accessed: now,
            expires_at: if self.ttl > 0 { Some(now + Duration::seconds(self.ttl)) } else { None },
            metadata: metadata.unwrap_or_default(),
            active_task_id: None,
            active_document_id: None,
            context: Map::new(),
            usage_stats: UsageStats::default(),
        };
        self.store.insert_seat(&seat).await?;
        Ok(seat)
    }

    pub async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
        self.store.get_seat(seat_id).await
    }

    /// Ensure a seat exists (create on miss) — used by the MCP auth path.
    pub async fn ensure_seat(&self, seat_id: &str) -> SlcResult<Seat> {
        if let Some(seat) = self.store.get_seat(seat_id).await? {
            self.store.touch_seat(seat_id).await?;
            return Ok(seat);
        }
        self.create_seat(Some(seat_id.to_string()), None, None).await
    }

    pub async fn list_active(&self, limit: usize) -> SlcResult<Vec<Seat>> {
        self.store.list_active_seats(limit).await
    }

    pub async fn close_seat(&self, seat_id: &str) -> SlcResult<bool> {
        self.store.set_seat_status(seat_id, SeatStatus::Closed).await
    }

    /// Mark expired seats as EXPIRED; returns how many.
    pub async fn cleanup_expired(&self) -> SlcResult<i64> {
        let now = Utc::now();
        let mut n = 0;
        for seat in self.store.list_active_seats(1000).await? {
            if let Some(exp) = seat.expires_at {
                if exp < now {
                    self.store.set_seat_status(&seat.seat_id, SeatStatus::Expired).await?;
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    pub async fn record_tool_use(&self, seat_id: &str, tool_name: &str, tokens: i64) -> SlcResult<bool> {
        self.store.incr_seat_stats(seat_id, tool_name, tokens).await
    }

    /// Active task pointer on the seat (working-memory context).
    pub async fn set_active_task(&self, seat_id: &str, task_id: Option<&str>, context: Option<Value>) -> SlcResult<bool> {
        let Some(mut seat) = self.store.get_seat(seat_id).await? else { return Ok(false) };
        seat.active_task_id = task_id.map(String::from);
        // A task is a document — keep the unified anchor in sync.
        seat.active_document_id = task_id.map(String::from);
        if let Some(ctx) = context {
            if let Some(obj) = ctx.as_object() {
                seat.context.extend(obj.clone());
            }
        }
        self.store.insert_seat(&seat).await?;
        Ok(true)
    }

    /// Set the seat's active document (any category) — the context anchor
    /// included in `update_context`. `None` clears it.
    pub async fn set_active_document(&self, seat_id: &str, document_id: Option<&str>) -> SlcResult<bool> {
        let Some(mut seat) = self.store.get_seat(seat_id).await? else { return Ok(false) };
        seat.active_document_id = document_id.map(String::from);
        self.store.insert_seat(&seat).await?;
        Ok(true)
    }

    /// The seat's active document id (unified field with task fallback).
    pub async fn get_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
        self.store.get_seat_active_document(seat_id).await
    }

    /// Set one key in the seat's context map (per-seat settings, e.g.
    /// `context_limit_chars`).
    pub async fn set_context_key(&self, seat_id: &str, key: &str, value: Value) -> SlcResult<bool> {
        let Some(mut seat) = self.store.get_seat(seat_id).await? else { return Ok(false) };
        seat.context.insert(key.to_string(), value);
        self.store.insert_seat(&seat).await?;
        Ok(true)
    }
}

/// Convenience: build the default metadata map from env (client/user ids).
pub fn seat_metadata_from_env() -> Map<String, Value> {
    let mut m = Map::new();
    if let Ok(client) = std::env::var("SLC_CLIENT_ID") {
        m.insert("client_id".into(), json!(client));
    }
    if let Ok(user) = std::env::var("SLC_USER_ID") {
        m.insert("user_id".into(), json!(user));
    }
    m
}

#[allow(dead_code)]
fn _assert_send_sync(_: &(dyn Send + Sync)) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    #[tokio::test]
    async fn seat_lifecycle() {
        let store = SqliteStore::in_memory().unwrap();
        let mgr = SeatManager::new(store, 60);

        let seat = mgr.create_seat(None, Some("cli".into()), None).await.unwrap();
        assert!(seat.seat_id.starts_with("slc_session_"));

        let named = mgr.create_seat(Some("cursor-1".into()), None, None).await.unwrap();
        assert_eq!(named.seat_id, "cursor-1");

        let ensured = mgr.ensure_seat("cursor-1").await.unwrap();
        assert_eq!(ensured.seat_id, "cursor-1");
        assert_eq!(mgr.list_active(10).await.unwrap().len(), 2);

        assert!(mgr.record_tool_use("cursor-1", "search", 10).await.unwrap());
        let s = mgr.get_seat("cursor-1").await.unwrap().unwrap();
        assert_eq!(s.usage_stats.total_requests, 1);

        assert!(mgr.close_seat("cursor-1").await.unwrap());
        assert_eq!(mgr.list_active(10).await.unwrap().len(), 1);
    }
}
