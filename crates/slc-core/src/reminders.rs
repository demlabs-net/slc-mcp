//! ReminderManager — CRUD for reminders + scheduling via TimerRegistry.
//!
//! Legacy equivalent: `src/reminders/manager.py` + `time_parser.py`. A
//! reminder schedules a one-shot `REMINDER` timer; when it fires, the handler
//! marks the reminder `fired` and pushes a notification.
//!
//! Stored as free-form JSON records (`collection = "reminders"`, keyed by
//! `reminder_id`).

use crate::error::{SlcError, SlcResult};
use crate::model::{Reminder, TimerType};
use crate::proactivity::{mind_matches, normalize_write_mind_type, MindType};
use crate::storage::StorageBackend;
use crate::timer::TimerRegistry;
use chrono::{DateTime, Utc};

pub const COLLECTION: &str = "reminders";

/// Hard cap on pending reminders per seat.
pub const MAX_REMINDERS_PER_SEAT: usize = 100;

/// CRUD + scheduling manager over the `reminders` records collection.
#[derive(Clone)]
pub struct ReminderManager<S: StorageBackend> {
    store: S,
    registry: TimerRegistry,
}

impl<S: StorageBackend> ReminderManager<S> {
    pub fn new(store: S, registry: TimerRegistry) -> Self {
        ReminderManager { store, registry }
    }

    pub async fn create(
        &self,
        seat_id: &str,
        content: &str,
        remind_at: DateTime<Utc>,
        user_id: Option<&str>,
        recurrence: Option<&str>,
        created_by_agent: bool,
        mind_type: Option<&str>,
    ) -> SlcResult<Reminder> {
        let mt = normalize_write_mind_type(mind_type).map_err(SlcError::InvalidInput)?;
        let pending = self.list(seat_id, Some("pending"), None).await?;
        if pending.len() >= MAX_REMINDERS_PER_SEAT {
            return Err(SlcError::Limit(format!(
                "Reminder limit reached ({MAX_REMINDERS_PER_SEAT})"
            )));
        }
        if remind_at <= Utc::now() {
            return Err(SlcError::InvalidInput(format!(
                "remind_at is in the past: {remind_at}"
            )));
        }

        let reminder_id = crate::model::unique_id("rem");
        let reminder = Reminder {
            reminder_id: reminder_id.clone(),
            seat_id: seat_id.into(),
            mind_type: mt,
            user_id: user_id.map(String::from),
            content: content.into(),
            remind_at,
            created_at: Utc::now(),
            status: "pending".into(),
            recurrence: recurrence.map(String::from),
            created_by_agent,
        };
        self.put(&reminder).await?;

        let mut meta = serde_json::Map::new();
        meta.insert("reminder_id".into(), serde_json::Value::String(reminder_id.clone()));
        self.registry
            .register(TimerType::Reminder, seat_id, None, Some(remind_at), serde_json::Value::Object(meta))
            .await?;
        Ok(reminder)
    }

    pub async fn cancel(&self, reminder_id: &str, seat_id: Option<&str>) -> SlcResult<bool> {
        let Some(mut r) = self.get(reminder_id, seat_id).await? else { return Ok(false) };
        if r.status != "pending" {
            return Ok(false);
        }
        r.status = "cancelled".into();
        self.put(&r).await?;
        self.registry
            .cancel_by_metadata(&r.seat_id, TimerType::Reminder, "reminder_id", reminder_id)
            .await?;
        Ok(true)
    }

    pub async fn get(&self, reminder_id: &str, seat_id: Option<&str>) -> SlcResult<Option<Reminder>> {
        let Some(val) = self.store.get_record(COLLECTION, reminder_id).await? else { return Ok(None) };
        let r: Reminder = serde_json::from_value(val)?;
        if let Some(s) = seat_id {
            if r.seat_id != s {
                return Ok(None);
            }
        }
        Ok(Some(r))
    }

    pub async fn list(&self, seat_id: &str, status: Option<&str>, mind_type: Option<MindType>) -> SlcResult<Vec<Reminder>> {
        let mut out = Vec::new();
        for (_, val) in self.store.list_records(COLLECTION).await? {
            if let Ok(r) = serde_json::from_value::<Reminder>(val) {
                if r.seat_id == seat_id
                    && status.map(|s| r.status == s).unwrap_or(true)
                    && mind_matches(r.mind_type, mind_type)
                {
                    out.push(r);
                }
            }
        }
        out.sort_by(|a, b| a.remind_at.cmp(&b.remind_at));
        Ok(out)
    }

    /// Mark a reminder as `fired` (called by the REMINDER timer handler).
    pub async fn mark_fired(&self, reminder_id: &str, seat_id: Option<&str>) -> SlcResult<bool> {
        let Some(mut r) = self.get(reminder_id, seat_id).await? else { return Ok(false) };
        if r.status != "pending" {
            return Ok(false);
        }
        r.status = "fired".into();
        self.put(&r).await?;
        Ok(true)
    }

    async fn put(&self, r: &Reminder) -> SlcResult<()> {
        self.store
            .put_record(COLLECTION, &r.reminder_id, &serde_json::to_value(r)?)
            .await
    }
}

/// Parse a `remind_at` from a string. Accepts ISO-8601/RFC3339 forms (the
/// reliable path; mirrors `_try_iso` in the legacy). Natural-language parsing
/// (`dateparser`) is not ported — NL strings return an error.
pub fn parse_remind_at(text: &str) -> SlcResult<DateTime<Utc>> {
    let text = text.trim();
    // RFC3339: 2026-08-13T10:00:00Z / +02:00 / no-fraction
    if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
        let utc = dt.with_timezone(&Utc);
        if utc > Utc::now() {
            return Ok(utc);
        }
        return Err(SlcError::InvalidInput(format!("Time is in the past: {utc}")));
    }
    // Naive forms (assume UTC, mirror legacy default timezone).
    for fmt in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, fmt)
            .or_else(|_| chrono::NaiveDate::parse_from_str(text, fmt).map(|d| d.and_hms_opt(0, 0, 0).unwrap()))
        {
            let dt = naive.and_utc();
            if dt > Utc::now() {
                return Ok(dt);
            }
            return Err(SlcError::InvalidInput(format!("Time is in the past: {dt}")));
        }
    }
    Err(SlcError::InvalidInput(format!("Cannot parse time: {text:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;
    use chrono::Duration;

    fn manager() -> (ReminderManager<std::sync::Arc<dyn StorageBackend>>, std::sync::Arc<dyn StorageBackend>) {
        let store: std::sync::Arc<dyn StorageBackend> = std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        let registry = TimerRegistry::new(store.clone());
        (ReminderManager::new(store.clone(), registry), store)
    }

    fn future(minutes: i64) -> DateTime<Utc> {
        Utc::now() + Duration::minutes(minutes)
    }

    #[tokio::test]
    async fn create_list_cancel() {
        let (m, _) = manager();
        let r = m.create("seat_r", "call dmitriy", future(30), None, None, false, None).await.unwrap();
        assert!(r.reminder_id.starts_with("rem_"));
        assert_eq!(m.list("seat_r", Some("pending"), None).await.unwrap().len(), 1);

        assert!(m.cancel(&r.reminder_id, None).await.unwrap());
        assert_eq!(m.list("seat_r", Some("pending"), None).await.unwrap().len(), 0);
        assert_eq!(m.list("seat_r", Some("cancelled"), None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_past_and_limit() {
        let (m, _) = manager();
        let past = Utc::now() - Duration::seconds(10);
        let err = m.create("seat_x", "in the past", past, None, None, false, None).await.unwrap_err();
        assert!(err.to_string().contains("past"), "{err}");

        for i in 0..MAX_REMINDERS_PER_SEAT {
            m.create("seat_l", &format!("r{i}"), future(60), None, None, false, None).await.unwrap();
        }
        let err = m.create("seat_l", "overflow", future(60), None, None, false, None).await.unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[tokio::test]
    async fn mark_fired_transitions() {
        let (m, _) = manager();
        let r = m.create("seat_f", "do it", future(10), None, None, false, None).await.unwrap();
        assert!(m.mark_fired(&r.reminder_id, None).await.unwrap());
        assert_eq!(m.get(&r.reminder_id, None).await.unwrap().unwrap().status, "fired");
        assert!(!m.mark_fired(&r.reminder_id, None).await.unwrap());
    }

    #[test]
    fn parse_time_iso() {
        let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
        assert!(parse_remind_at(&future).is_ok());
        assert!(parse_remind_at("not a time").is_err());
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        assert!(parse_remind_at(&past).is_err());
    }
}
