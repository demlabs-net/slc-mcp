//! Persisted timer scheduler — the "heartbeat" of the memory engine.
//!
//! Legacy equivalent: `src/timers/registry.py` + `defaults.py`. Timers are
//! persisted rows (one-shot or periodic); the registry spawns a tokio task
//! per active timer that sleeps until `next_fire_at`, marks `last_fired_at`,
//! calls the registered handler, then reschedules periodic timers
//! (`interval_seconds`) or deactivates one-shots. On process start
//! (`TimerRegistry::start`) all active timers are reloaded from storage.

use crate::error::SlcResult;
use crate::model::{PersistedTimer, TimerType};
use crate::storage::StorageBackend;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration as StdDuration;

/// A timer handler — called when the timer fires.
#[async_trait]
pub trait TimerHandler: Send + Sync {
    async fn handle(&self, timer: &PersistedTimer) -> SlcResult<()>;
}

/// Default per-seat timer intervals (legacy `defaults.py` env keys).
pub fn default_interval(timer_type: TimerType) -> i64 {
    let (key, fallback) = match timer_type {
        TimerType::FocusReminder => ("FOCUS_REMINDER_INTERVAL_SEC", 900),
        TimerType::HistoryCompression => ("HISTORY_COMPRESSION_INTERVAL_SEC", 86400),
        TimerType::Consolidation => ("CONSOLIDATION_INTERVAL_SEC", 86400),
        TimerType::Reminder => return 0, // reminder timers are one-shot
    };
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

/// All periodic timer types created per seat on first use.
pub const DEFAULT_PERIODIC_TIMERS: [TimerType; 3] = [
    TimerType::FocusReminder,
    TimerType::HistoryCompression,
    TimerType::Consolidation,
];

/// Countdown-until-fired registry. Clonable (shares store/handlers/tasks).
#[derive(Clone)]
pub struct TimerRegistry {
    store: Arc<dyn StorageBackend>,
    handlers: Arc<std::sync::RwLock<HashMap<TimerType, Arc<dyn TimerHandler>>>>,
    tasks: Arc<std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    spawned: Arc<AtomicUsize>,
}

impl TimerRegistry {
    pub fn new(store: Arc<dyn StorageBackend>) -> Self {
        TimerRegistry {
            store,
            handlers: Arc::new(std::sync::RwLock::new(HashMap::new())),
            tasks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            spawned: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn set_handler(&self, timer_type: TimerType, handler: Arc<dyn TimerHandler>) {
        let mut h = self.handlers.write().unwrap();
        h.insert(timer_type, handler);
    }

    /// Register a timer. `interval_seconds: None` = one-shot;
    /// `next_fire_at: None` = now + interval (or now for one-shots).
    pub async fn register(
        &self,
        timer_type: TimerType,
        seat_id: &str,
        interval_seconds: Option<i64>,
        next_fire_at: Option<DateTime<Utc>>,
        metadata: Value,
    ) -> SlcResult<String> {
        let now = Utc::now();
        let timer = PersistedTimer {
            timer_id: crate::model::unique_id("tmr"),
            seat_id: seat_id.to_string(),
            timer_type,
            interval_seconds,
            last_fired_at: None,
            next_fire_at: next_fire_at
                .unwrap_or_else(|| now + chrono::Duration::seconds(interval_seconds.unwrap_or(0))),
            is_active: true,
            metadata: metadata.as_object().cloned().unwrap_or_default(),
            created_at: now,
        };
        self.store.insert_timer(&timer).await?;
        self.spawn(timer.clone());
        Ok(timer.timer_id)
    }

    /// Create the 5 default periodic timers for a seat (idempotent per seat:
    /// skips types that already have an active timer).
    pub async fn create_defaults(&self, seat_id: &str) -> SlcResult<Vec<String>> {
        let existing = self.store.active_timers(Some(seat_id)).await?;
        let mut created = Vec::new();
        for t in DEFAULT_PERIODIC_TIMERS {
            if existing.iter().any(|e| e.timer_type == t) {
                continue;
            }
            let interval = default_interval(t);
            if interval <= 0 {
                continue;
            }
            let id = self
                .register(t, seat_id, Some(interval), None, Value::Null)
                .await?;
            created.push(id);
        }
        Ok(created)
    }

    /// Load all active timers and spawn their tasks (startup).
    pub async fn start(&self) -> SlcResult<()> {
        for timer in self.store.active_timers(None).await? {
            self.spawn(timer);
        }
        Ok(())
    }

    /// Abort every spawned task.
    pub async fn stop(&self) {
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for (_, handle) in tasks {
            handle.abort();
        }
    }

    /// Cancel: deactivate in storage + abort the task.
    pub async fn cancel(&self, timer_id: &str) -> SlcResult<bool> {
        let Some(mut timer) = self.store.get_timer(timer_id).await? else {
            return Ok(false);
        };
        timer.is_active = false;
        self.store.insert_timer(&timer).await?;
        if let Some(handle) = self.tasks.lock().unwrap().remove(timer_id) {
            handle.abort();
        }
        Ok(true)
    }

    /// Cancel all active timers for a seat whose metadata key == value
    /// (e.g. `reminder_id`). Returns the number cancelled.
    pub async fn cancel_by_metadata(
        &self,
        seat_id: &str,
        timer_type: TimerType,
        key: &str,
        value: &str,
    ) -> SlcResult<usize> {
        let timers = self.store.active_timers(Some(seat_id)).await?;
        let mut cancelled = 0;
        for timer in timers {
            let is_type = timer.timer_type == timer_type;
            let matches = timer.metadata.get(key).and_then(|v| v.as_str()) == Some(value);
            if is_type && matches && self.cancel(&timer.timer_id).await? {
                cancelled += 1;
            }
        }
        Ok(cancelled)
    }

    pub async fn list(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
        self.store.active_timers(seat_id).await
    }

    /// Number of spawned tasks (tests).
    pub fn spawned_count(&self) -> usize {
        self.spawned.load(Ordering::Relaxed)
    }

    fn spawn(&self, timer: PersistedTimer) {
        let store = self.store.clone();
        let handlers = self.handlers.clone();
        let spawned = self.spawned.clone();
        let timer_id = timer.timer_id.clone();
        if self.tasks.lock().unwrap().contains_key(&timer_id) {
            return;
        }
        let handle = tokio::spawn(async move {
            let mut timer = timer;
            loop {
                let now = Utc::now();
                if timer.next_fire_at > now {
                    let dur = (timer.next_fire_at - now)
                        .to_std()
                        .unwrap_or(StdDuration::ZERO);
                    tokio::time::sleep(dur).await;
                }
                let _ = store.set_timer_fired(&timer.timer_id, Utc::now()).await;
                timer.last_fired_at = Some(Utc::now());
                // Clone the handler OUT of the lock guard before awaiting.
                let handler = handlers.read().unwrap().get(&timer.timer_type).cloned();
                if let Some(handler) = handler {
                    if let Err(e) = handler.handle(&timer).await {
                        tracing::warn!(
                            "timer {} ({}) handler error: {e}",
                            timer.timer_id,
                            timer.timer_type.as_str()
                        );
                    }
                }
                match timer.interval_seconds.filter(|i| *i > 0) {
                    Some(interval) => {
                        // Periodic: recompute next fire, persist (upsert), continue.
                        timer.next_fire_at = Utc::now() + chrono::Duration::seconds(interval);
                        let _ = store.insert_timer(&timer).await;
                    }
                    None => {
                        // One-shot: deactivate and exit.
                        timer.is_active = false;
                        let _ = store.insert_timer(&timer).await;
                        break;
                    }
                }
            }
        });
        self.tasks.lock().unwrap().insert(timer_id, handle);
        spawned.fetch_add(1, Ordering::Relaxed);
    }
}

/// Generic closure-style handler adapter (tests, wiring).
pub struct FnHandler<F> {
    f: F,
}

impl<F> FnHandler<F>
where
    F: Fn(&PersistedTimer) -> SlcResult<()> + Send + Sync,
{
    pub fn new(f: F) -> Self {
        FnHandler { f }
    }
}

#[async_trait]
impl<F> TimerHandler for FnHandler<F>
where
    F: Fn(&PersistedTimer) -> SlcResult<()> + Send + Sync,
{
    async fn handle(&self, timer: &PersistedTimer) -> SlcResult<()> {
        (self.f)(timer)
    }
}

/// Async closure handler (e.g. compress/consolidate which are async).
pub struct AsyncFnHandler<F> {
    f: F,
}

impl<F> AsyncFnHandler<F>
where
    F: Fn(
            &PersistedTimer,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SlcResult<()>> + Send>>
        + Send
        + Sync,
{
    pub fn new(f: F) -> Self {
        AsyncFnHandler { f }
    }
}

#[async_trait]
impl<F> TimerHandler for AsyncFnHandler<F>
where
    F: Fn(
            &PersistedTimer,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SlcResult<()>> + Send>>
        + Send
        + Sync,
{
    async fn handle(&self, timer: &PersistedTimer) -> SlcResult<()> {
        (self.f)(timer).await
    }
}

// ─────────────────────────────── tests ───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;
    use tokio::sync::Notify;

    /// Yield repeatedly so a timer task can finish its storage update after
    /// notifying the test handler.
    async fn yield_a_bit() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn periodic_timer_fires_and_reschedules() {
        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let registry = TimerRegistry::new(store.clone());
        let fired = Arc::new(AtomicUsize::new(0));
        let fired2 = fired.clone();
        let signal = Arc::new(Notify::new());
        let signal2 = signal.clone();
        registry.set_handler(
            TimerType::HistoryCompression,
            Arc::new(FnHandler::new(move |_t| {
                fired2.fetch_add(1, Ordering::Relaxed);
                signal2.notify_one();
                Ok(())
            })),
        );

        let tid = registry
            .register(
                TimerType::HistoryCompression,
                "seat_t",
                Some(1),
                Some(Utc::now() - chrono::Duration::seconds(1)),
                Value::Null,
            )
            .await
            .unwrap();
        let _ = registry.start().await;
        assert_eq!(registry.spawned_count(), 1);

        // Start overdue so awaiting the handler itself is the readiness
        // barrier; this avoids racing a forced clock advance against spawn.
        signal.notified().await;
        assert_eq!(fired.load(Ordering::Relaxed), 1, "first fire");
        yield_a_bit().await;

        // periodic → still active and rescheduled
        let active = registry.list(Some("seat_t")).await.unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].is_active);
        assert!(active[0].last_fired_at.is_some());

        // The first handler has completed and the periodic task has installed
        // its next sleep, so advancing now exercises the reschedule path.
        tokio::time::advance(StdDuration::from_secs(2)).await;
        signal.notified().await;
        assert!(fired.load(Ordering::Relaxed) >= 2, "rescheduled fire");

        assert!(registry.cancel(&tid).await.unwrap());
        assert!(
            registry.list(Some("seat_t")).await.unwrap().is_empty(),
            "cancelled → inactive"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn one_shot_deactivates_after_fire() {
        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let registry = TimerRegistry::new(store.clone());
        let fired = Arc::new(AtomicUsize::new(0));
        let fired2 = fired.clone();
        let signal = Arc::new(Notify::new());
        let signal2 = signal.clone();
        registry.set_handler(
            TimerType::Reminder,
            Arc::new(FnHandler::new(move |_t| {
                fired2.fetch_add(1, Ordering::Relaxed);
                signal2.notify_one();
                Ok(())
            })),
        );
        let _ = registry
            .register(
                TimerType::Reminder,
                "seat_o",
                None,
                Some(Utc::now() - chrono::Duration::seconds(1)),
                Value::Null,
            )
            .await
            .unwrap();
        let _ = registry.start().await;
        signal.notified().await;
        assert_eq!(fired.load(Ordering::Relaxed), 1);
        let mut active = registry.list(Some("seat_o")).await.unwrap();
        for _ in 0..128 {
            if active.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
            active = registry.list(Some("seat_o")).await.unwrap();
        }
        assert!(active.is_empty(), "one-shot done → inactive");
    }

    #[tokio::test]
    async fn defaults_created_once_per_seat() {
        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let registry = TimerRegistry::new(store.clone());
        let created = registry.create_defaults("seat_d").await.unwrap();
        assert_eq!(created.len(), 3, "all three periodic defaults");
        let again = registry.create_defaults("seat_d").await.unwrap();
        assert!(again.is_empty(), "idempotent");
        assert_eq!(registry.list(Some("seat_d")).await.unwrap().len(), 3);
    }

    #[test]
    fn interval_env_fallbacks() {
        assert_eq!(default_interval(TimerType::HistoryCompression), 86400);
        assert_eq!(default_interval(TimerType::FocusReminder), 900);
        assert_eq!(default_interval(TimerType::Consolidation), 86400);
    }

    fn _unused(_: crate::error::SlcError) {}
}
