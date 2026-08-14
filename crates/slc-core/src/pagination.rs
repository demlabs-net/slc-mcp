//! Pagination — split oversized tool responses into pages and fetch them.
//!
//! Legacy equivalent: `src/slc_mcp/paginated_cache.py` (subset). When a tool
//! response is too large it is cached as a list of pages (stored as free-form
//! JSON records); the inline result carries a `_pagination` envelope
//! (`response_id`, `page`, `total_pages`). `get_page` retrieves later pages.
//!
//! Stored records (collection `paginated_responses`):
//! - `{response_id}:pages` — array of page values
//! - `{response_id}:meta` — `{total_pages, page_token_limit, created_at}`

use crate::error::SlcResult;
use crate::storage::StorageBackend;
use chrono::{Duration, Utc};
use serde_json::{json, Value};

pub const COLLECTION: &str = "paginated_responses";

/// Min / max page sizes in tokens (legacy constants).
pub const MIN_PAGE_TOKEN_LIMIT: usize = 500;
pub const MAX_PAGE_TOKEN_LIMIT: usize = 100_000;
pub const DEFAULT_PAGE_TOKEN_LIMIT: usize = 16_000;

/// Rough chars-per-token estimate (legacy CHARS_PER_TOKEN=4).
pub const CHARS_PER_TOKEN: usize = 4;
/// Auto-delete responses after this long.
pub const TTL_SECONDS: i64 = 600; // 10 min

/// Page-splitting + retrieval, backed by the records store.
#[derive(Clone)]
pub struct Paginator<S: StorageBackend> {
    store: S,
}

impl<S: StorageBackend> Paginator<S> {
    pub fn new(store: S) -> Self {
        Paginator { store }
    }

    fn page_token_limit(&self) -> usize {
        std::env::var("SLC_PAGE_TOKEN_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(|v: usize| v.clamp(MIN_PAGE_TOKEN_LIMIT, MAX_PAGE_TOKEN_LIMIT))
            .unwrap_or(DEFAULT_PAGE_TOKEN_LIMIT)
    }

    /// Cache a response, splitting it into pages. Returns the paginated
    /// result (with `_pagination`). If the payload fits one page, returns it
    /// directly with a single-page envelope.
    pub async fn paginate(&self, _seat_id: &str, response_id: &str, data: &Value) -> SlcResult<Value> {
        // `data` is expected to be an object; if it is an array, wrap it.
        let result = if data.is_array() {
            json!({ "content": data })
        } else {
            data.clone()
        };
        let list_key = find_list_key(&result);
        let char_limit = self.page_token_limit() * CHARS_PER_TOKEN;

        if list_key.is_none() {
            // Not a list — single page, still tagged.
            let mut out = result.clone();
            out["_pagination"] = json!({ "response_id": response_id, "page": 1, "total_pages": 1 });
            return Ok(out);
        }

        let items = result[list_key.as_ref().unwrap()].as_array().cloned().unwrap_or_default();
        let total_items = items.len();
        let envelope: Value = {
            let mut e = result.clone();
            e.as_object_mut().unwrap().remove(list_key.as_ref().unwrap());
            e
        };

        // Greedy pack items into pages by estimated chars.
        let mut pages: Vec<Value> = Vec::new();
        let mut current: Vec<Value> = Vec::new();
        let mut current_chars = 0usize;
        for item in items {
            let item_chars = estimate_chars(&item);
            if !current.is_empty() && current_chars + item_chars > char_limit {
                let mut page = envelope.clone();
                page[list_key.as_ref().unwrap()] = json!(current);
                pages.push(page);
                current = Vec::new();
                current_chars = 0;
            }
            current.push(item);
            current_chars += item_chars;
        }
        if !current.is_empty() {
            let mut page = envelope.clone();
            page[list_key.as_ref().unwrap()] = json!(current);
            pages.push(page);
        }
        if pages.is_empty() {
            let mut page = envelope.clone();
            page[list_key.as_ref().unwrap()] = json!([]);
            pages.push(page);
        }

        let total_pages = pages.len();
        self.store
            .put_record(COLLECTION, &format!("{response_id}:pages"), &json!(pages))
            .await?;
        self.store
            .put_record(
                COLLECTION,
                &format!("{response_id}:meta"),
                &json!({
                    "total_pages": total_pages,
                    "total_items": total_items,
                    "page_token_limit": self.page_token_limit(),
                    "created_at": Utc::now().to_rfc3339(),
                }),
            )
            .await?;

        // First page inline.
        let mut first = pages.into_iter().next().unwrap_or_default();
        first["_pagination"] = json!({
            "response_id": response_id,
            "page": 1,
            "total_pages": total_pages,
            "total_items": total_items,
        });
        Ok(first)
    }

    /// Fetch a specific page.
    pub async fn get_page(&self, seat_id: &str, response_id: &str, page: usize) -> SlcResult<Value> {
        let _ = seat_id;
        let Some(pages_val) = self.store.get_record(COLLECTION, &format!("{response_id}:pages")).await? else {
            return Ok(json!({ "error": format!("Response '{response_id}' not found or already expired") }));
        };
        let pages = pages_val.as_array().cloned().unwrap_or_default();
        if page < 1 || page > pages.len() {
            return Ok(json!({ "error": format!("Invalid page {page}; total pages {}", pages.len()) }));
        }
        let mut out = pages[page - 1].clone();
        out["_pagination"] = json!({
            "response_id": response_id,
            "page": page,
            "total_pages": pages.len(),
        });
        Ok(out)
    }

    /// Delete a cached response.
    pub async fn delete_response(&self, response_id: &str) -> SlcResult<Value> {
        let pages_removed = self.store.delete_record(COLLECTION, &format!("{response_id}:pages")).await?;
        self.store.delete_record(COLLECTION, &format!("{response_id}:meta")).await?;
        if !pages_removed {
            return Ok(json!({ "error": format!("Response '{response_id}' not found or already expired") }));
        }
        Ok(json!({ "success": true, "response_id": response_id, "deleted_pages": 1 }))
    }

    pub async fn set_page_limit(&self, tokens: usize) -> SlcResult<Value> {
        if tokens < MIN_PAGE_TOKEN_LIMIT {
            return Ok(json!({ "error": format!("Minimum page limit is {MIN_PAGE_TOKEN_LIMIT} tokens") }));
        }
        if tokens > MAX_PAGE_TOKEN_LIMIT {
            return Ok(json!({ "error": format!("Maximum page limit is {MAX_PAGE_TOKEN_LIMIT} tokens") }));
        }
        // Persist under canonical + production-compat keys.
        let mut m = serde_json::Map::new();
        m.insert("page_token_limit".into(), json!(tokens));
        self.store.put_record(COLLECTION, "settings", &Value::Object(m.clone())).await?;
        m.insert("limit_tokens".into(), json!(tokens));
        self.store.put_record(COLLECTION, "settings:prod", &Value::Object(m)).await?;
        Ok(json!({ "success": true, "page_token_limit": tokens, "limit_tokens": tokens }))
    }

    pub async fn get_settings(&self) -> SlcResult<Value> {
        Ok(json!({
            "page_token_limit": self.page_token_limit(),
            "chars_per_token": CHARS_PER_TOKEN,
            "ttl_seconds": TTL_SECONDS,
        }))
    }

    /// Auto-expire old responses (call periodically).
    pub async fn cleanup(&self, ttl_seconds: i64) -> SlcResult<usize> {
        let cutoff = Utc::now() - Duration::seconds(ttl_seconds);
        let mut removed = 0;
        for (key, val) in self.store.list_records(COLLECTION).await? {
            if key.ends_with(":meta") {
                let created = val.get("created_at").and_then(|v| v.as_str()).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                if let Some(created) = created {
                    if created.with_timezone(&Utc) < cutoff {
                        let base = key.trim_end_matches(":meta");
                        if self.store.delete_record(COLLECTION, &format!("{base}:pages")).await? {
                            removed += 1;
                        }
                        self.store.delete_record(COLLECTION, &key).await?;
                    }
                }
            }
        }
        Ok(removed)
    }
}

/// Rough character estimate for an item (serialized JSON).
fn estimate_chars(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

/// First key whose value is a non-empty list.
fn find_list_key(result: &Value) -> Option<String> {
    let obj = result.as_object()?;
    for key in ["results", "content", "items", "focuses", "reminders", "notifications", "tasks", "projects", "docs", "events"] {
        if let Some(v) = obj.get(key) {
            if v.is_array() {
                return Some(key.to_string());
            }
        }
    }
    for (k, v) in obj {
        if v.is_array() {
            return Some(k.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    fn paginator() -> (Paginator<std::sync::Arc<dyn StorageBackend>>, std::sync::Arc<dyn StorageBackend>) {
        let store: std::sync::Arc<dyn StorageBackend> = std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (Paginator::new(store.clone()), store)
    }

    #[tokio::test]
    async fn paginates_a_list() {
        let (p, _) = paginator();
        unsafe { std::env::set_var("SLC_PAGE_TOKEN_LIMIT", "500") };
        let items: Vec<Value> = (0..100).map(|i| json!({"i": i, "text": "x".repeat(50)})).collect();
        let result = p.paginate("seat_x", "resp_1", &json!({ "results": items })).await.unwrap();
        let total_pages = result["_pagination"]["total_pages"].as_i64().unwrap();
        assert!(total_pages > 1, "expected multiple pages, got {total_pages}");
        assert_eq!(result["_pagination"]["page"], 1);

        // fetch a middle page
        let page2 = p.get_page("seat_x", "resp_1", 2).await.unwrap();
        assert!(page2.get("_pagination").is_some());
        assert!(page2.get("results").is_some());

        assert!(p.delete_response("resp_1").await.unwrap()["success"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn single_page_no_split() {
        let (p, _) = paginator();
        let small: Vec<Value> = (0..3).map(|i| json!({"i": i})).collect();
        let result = p.paginate("seat_y", "resp_s", &json!({ "items": small })).await.unwrap();
        assert_eq!(result["_pagination"]["total_pages"], 1);
    }

    #[tokio::test]
    async fn invalid_page_errors() {
        let (p, _) = paginator();
        let small: Vec<Value> = vec![json!(1), json!(2)];
        p.paginate("seat_z", "resp_e", &json!({ "items": small })).await.unwrap();
        let err = p.get_page("seat_z", "resp_e", 99).await.unwrap();
        assert!(err.get("error").is_some());
    }

    #[tokio::test]
    async fn page_limit_bounds() {
        let (p, _) = paginator();
        assert!(p.set_page_limit(10).await.unwrap()["error"].is_string());
        assert!(p.set_page_limit(500).await.unwrap()["success"].as_bool().unwrap());
    }
}
