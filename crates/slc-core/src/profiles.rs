//! User and seat profiles — procedural memory about the user/workspace.
//!
//! Legacy equivalent: `src/personalization/manager.py` (UserProfileManager /
//! SeatProfileManager). A profile is a single Document per identity
//! (`category = System`, `doc_type = USER_PROFILE` / `SEAT_PROFILE`) holding
//! the markdown content plus a timezone for seats.

use crate::error::SlcResult;
use crate::model::{DocMeta, Document, DocumentCategory};
use crate::storage::{DocFilter, SortDir, StorageBackend};
use serde_json::{Value, json};

const USER_PREFIX: &str = "user_profile_";
const SEAT_PREFIX: &str = "seat_profile_";

/// User/seat profile manager backed by the KB.
#[derive(Clone)]
pub struct ProfileManager<S: StorageBackend> {
    store: S,
}

impl<S: StorageBackend> ProfileManager<S> {
    pub fn new(store: S) -> Self {
        ProfileManager { store }
    }

    /// Get the user profile doc id.
    pub fn user_id(user_id: &str) -> String {
        format!("{USER_PREFIX}{user_id}")
    }
    pub fn seat_id(seat_id: &str) -> String {
        format!("{SEAT_PREFIX}{seat_id}")
    }

    fn doc(doc_id: String, doc_type: &str, content: &str, seat_id: &str) -> Document {
        let mut meta = DocMeta::default();
        meta.doc_type = Some(doc_type.into());
        meta.seat_id = Some(seat_id.into());
        Document::new(
            doc_id,
            DocumentCategory::System,
            content,
            meta,
            vec!["profile".into()],
            Some(seat_id.into()),
        )
    }

    pub async fn get_user_profile(
        &self,
        user_id: &str,
        seat_id: &str,
    ) -> SlcResult<Option<String>> {
        let _ = seat_id;
        let id = Self::user_id(user_id);
        Ok(self.store.kb_get(&id).await?.map(|d| d.content))
    }

    pub async fn upsert_user_profile(
        &self,
        user_id: &str,
        content: &str,
        seat_id: &str,
    ) -> SlcResult<bool> {
        let id = Self::user_id(user_id);
        let existed = self.store.kb_get(&id).await?.is_some();
        let doc = Self::doc(id, "USER_PROFILE", content, seat_id);
        self.store.kb_replace(&doc).await?;
        Ok(existed)
    }

    pub async fn get_seat_profile(&self, seat_id: &str) -> SlcResult<Option<(String, String)>> {
        let id = Self::seat_id(seat_id);
        let Some(doc) = self.store.kb_get(&id).await? else {
            return Ok(None);
        };
        let tz = doc
            .metadata
            .extra
            .get("timezone")
            .and_then(|v| v.as_str())
            .unwrap_or("UTC")
            .to_string();
        Ok(Some((doc.content, tz)))
    }

    pub async fn upsert_seat_profile(
        &self,
        seat_id: &str,
        content: &str,
        timezone: Option<&str>,
    ) -> SlcResult<bool> {
        let id = Self::seat_id(seat_id);
        let existed = self.store.kb_get(&id).await?.is_some();
        let mut doc = Self::doc(id, "SEAT_PROFILE", content, seat_id);
        doc.metadata
            .extra
            .insert("timezone".into(), json!(timezone.unwrap_or("UTC")));
        self.store.kb_replace(&doc).await?;
        Ok(existed)
    }

    /// User id resolved from the seat's metadata (fallback to seat id).
    pub async fn resolve_user_id(&self, seat_id: &str) -> SlcResult<String> {
        Ok(self
            .store
            .get_seat(seat_id)
            .await?
            .and_then(|s| {
                s.metadata
                    .get("user_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| seat_id.to_string()))
    }

    #[allow(dead_code)]
    async fn _all_profiles(&self, doc_type: &str, seat_id: &str) -> SlcResult<Vec<Document>> {
        let filter = DocFilter {
            category: Some(DocumentCategory::System),
            seat_id: Some(seat_id.into()),
            ..Default::default()
        };
        let mut docs = self
            .store
            .kb_find(
                &filter,
                &crate::storage::DocSort::by_created(SortDir::Asc),
                100,
            )
            .await?;
        docs.retain(|d| d.metadata.doc_type.as_deref() == Some(doc_type));
        let _ = Value::Null;
        Ok(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    fn mgr() -> (
        ProfileManager<std::sync::Arc<dyn StorageBackend>>,
        std::sync::Arc<dyn StorageBackend>,
    ) {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (ProfileManager::new(store.clone()), store)
    }

    #[tokio::test]
    async fn user_profile_create_update_read() {
        let (m, _) = mgr();
        assert!(
            m.get_user_profile("alice", "seat_u")
                .await
                .unwrap()
                .is_none()
        );
        m.upsert_user_profile("alice", "Likes concise replies", "seat_u")
            .await
            .unwrap();
        assert_eq!(
            m.get_user_profile("alice", "seat_u")
                .await
                .unwrap()
                .unwrap(),
            "Likes concise replies"
        );
        m.upsert_user_profile("alice", "Likes JSON", "seat_u")
            .await
            .unwrap();
        assert_eq!(
            m.get_user_profile("alice", "seat_u")
                .await
                .unwrap()
                .unwrap(),
            "Likes JSON"
        );
    }

    #[tokio::test]
    async fn seat_profile_timezone() {
        let (m, _) = mgr();
        assert!(m.get_seat_profile("seat_s").await.unwrap().is_none());
        m.upsert_seat_profile("seat_s", "Moscow workspace", Some("Europe/Moscow"))
            .await
            .unwrap();
        let (content, tz) = m.get_seat_profile("seat_s").await.unwrap().unwrap();
        assert_eq!(content, "Moscow workspace");
        assert_eq!(tz, "Europe/Moscow");
    }
}
