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
use serde_json::{Value, json};

pub const COLLECTION: &str = "paginated_responses";

/// Default page size. Clients with a smaller result budget can override it
/// per connection with `X-SLC-Page-Token-Limit`.
pub const DEFAULT_PAGE_TOKEN_LIMIT: usize = 50_000;

/// Rough chars-per-token estimate (RU/EN смесь, как в движке).
pub const CHARS_PER_TOKEN: usize = 3;
/// Auto-delete responses after this long.
pub const TTL_SECONDS: i64 = 600; // 10 min

/// Global pagination default. Individual MCP connections may override this
/// through transport metadata without mutating server-wide state.
pub fn pagination_enabled_from_env() -> bool {
    std::env::var("SLC_PAGINATION_ENABLED")
        .ok()
        .and_then(|value| parse_bool(&value))
        .unwrap_or(true)
}

/// Parse and clamp the operator-provided page size. Returning `None` keeps
/// persisted settings usable when the environment variable is absent.
pub fn page_token_limit_from_env() -> Option<usize> {
    std::env::var("SLC_PAGE_TOKEN_LIMIT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(normalize_page_token_limit)
}

pub fn normalize_page_token_limit(tokens: usize) -> usize {
    // The operator/client owns its transport budget. Only zero is invalid;
    // do not impose a model-window policy or silently cap large contexts.
    tokens.max(1)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

/// Page-splitting + retrieval, backed by the records store.
#[derive(Clone)]
pub struct Paginator<S: StorageBackend> {
    store: S,
}

impl<S: StorageBackend> Paginator<S> {
    pub fn new(store: S) -> Self {
        Paginator { store }
    }

    async fn stored_page_token_limit(&self) -> SlcResult<Option<usize>> {
        for key in ["settings", "settings:prod"] {
            let Some(settings) = self.store.get_record(COLLECTION, key).await? else {
                continue;
            };
            let value = settings
                .get("page_token_limit")
                .or_else(|| settings.get("limit_tokens"))
                .and_then(Value::as_u64)
                .map(|tokens| normalize_page_token_limit(tokens as usize));
            if value.is_some() {
                return Ok(value);
            }
        }
        Ok(None)
    }

    pub async fn page_token_limit(&self) -> SlcResult<usize> {
        if let Some(tokens) = page_token_limit_from_env() {
            return Ok(tokens);
        }
        Ok(self
            .stored_page_token_limit()
            .await?
            .unwrap_or(DEFAULT_PAGE_TOKEN_LIMIT))
    }

    /// Разбить item по содержимому (`content`) на части ≤ `char_limit`
    /// символов. Каждая часть помечается `part: "k/n"`; клиент склеивает
    /// их по порядку. Не обрезка — все части отдаются через `get_page`.
    fn split_item(item: &Value, char_limit: usize) -> Vec<Value> {
        let Some(content) = item.get("content").and_then(|v| v.as_str()) else {
            return vec![item.clone()];
        };
        let total = content.chars().count();
        if total <= char_limit {
            return vec![item.clone()];
        }
        let mut base = item.clone();
        if let Some(obj) = base.as_object_mut() {
            obj.remove("content");
        }
        let n_parts = total.div_ceil(char_limit);
        let mut parts = Vec::new();
        let mut rest = content;
        let mut part = 1;
        while !rest.is_empty() {
            let take: String = rest.chars().take(char_limit).collect();
            let taken_bytes = take.len();
            let mut p = base.clone();
            p["content"] = json!(take);
            p["part"] = json!(format!("{part}/{n_parts}"));
            parts.push(p);
            rest = &rest[taken_bytes..];
            part += 1;
        }
        parts
    }

    /// Cache a response, splitting it into pages. Returns the paginated
    /// result (with `_pagination`). If the payload fits one page, returns it
    /// directly with a single-page envelope.
    pub async fn paginate(
        &self,
        _seat_id: &str,
        response_id: &str,
        data: &Value,
    ) -> SlcResult<Value> {
        let page_token_limit = self.page_token_limit().await?;
        self.paginate_with_limit(_seat_id, response_id, data, page_token_limit)
            .await
    }

    /// Paginate with an explicit per-connection page size. This is used by
    /// MCP transport metadata and keeps one client from changing another
    /// client's response shape.
    pub async fn paginate_with_limit(
        &self,
        _seat_id: &str,
        response_id: &str,
        data: &Value,
        page_token_limit: usize,
    ) -> SlcResult<Value> {
        let page_token_limit = normalize_page_token_limit(page_token_limit);
        // `data` is expected to be an object; if it is an array, wrap it.
        let result = if data.is_array() {
            json!({ "content": data })
        } else {
            data.clone()
        };
        let list_key = find_list_key(&result);
        let char_limit = page_token_limit.saturating_mul(CHARS_PER_TOKEN);
        // Часть контента никогда не меньше 200 символов: защита от
        // underflow (char_limit < 200) и от тысяч микро-страниц при
        // экстремально малых лимитах.
        let chunk_limit = char_limit.saturating_sub(200).max(200);

        if list_key.is_none() {
            // Одиночный объект (например, get_document): если у него
            // большой строковый `content` — режем ПО СОДЕРЖИМОМУ на части
            // (поле `part: "k/n"`), каждая часть — страница get_page;
            // клиент склеивает по порядку part — полный контент доходит
            // без потерь даже при жёстком бюджете обвязки клиента.
            if let Some(content) = result.get("content").and_then(|v| v.as_str()) {
                if content.chars().count() > char_limit {
                    let parts = Self::split_item(&result, chunk_limit);
                    let total_pages = parts.len();
                    self.store
                        .put_record(COLLECTION, &format!("{response_id}:pages"), &json!(parts))
                        .await?;
                    self.store
                        .put_record(
                            COLLECTION,
                            &format!("{response_id}:meta"),
                            &json!({
                                "total_pages": total_pages,
                                "total_items": 1,
                                "page_token_limit": page_token_limit,
                                "created_at": Utc::now().to_rfc3339(),
                            }),
                        )
                        .await?;
                    let mut first = parts.into_iter().next().unwrap_or_default();
                    first["_pagination"] = page_metadata(response_id, 1, total_pages, Some(1));
                    return Ok(first);
                }
            }
            // Not a list — single page, still tagged.
            let mut out = result.clone();
            out["_pagination"] = page_metadata(response_id, 1, 1, None);
            return Ok(out);
        }

        let items = result[list_key.as_ref().unwrap()]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let total_items = items.len();
        let envelope: Value = {
            let mut e = result.clone();
            e.as_object_mut()
                .unwrap()
                .remove(list_key.as_ref().unwrap());
            e
        };

        // Greedy pack items into pages by estimated chars.
        let mut pages: Vec<Value> = Vec::new();
        let mut current: Vec<Value> = Vec::new();
        let mut current_chars = 0usize;
        let flush =
            |pages: &mut Vec<Value>, current: &mut Vec<Value>, current_chars: &mut usize| {
                if !current.is_empty() {
                    let mut page = envelope.clone();
                    page[list_key.as_ref().unwrap()] = json!(*current);
                    pages.push(page);
                    *current = Vec::new();
                    *current_chars = 0;
                }
            };
        for item in items {
            let item_chars = estimate_chars(&item);
            if item_chars > char_limit {
                // Один документ не влезает в страницу — режем ПО
                // СОДЕРЖИМОМУ на части (не обрезка: каждая часть — своя
                // страница, клиент забирает все через get_page и склеивает
                // по `part k/n`).
                flush(&mut pages, &mut current, &mut current_chars);
                for part in Self::split_item(&item, chunk_limit) {
                    flush(&mut pages, &mut current, &mut current_chars);
                    current.push(part);
                    current_chars = estimate_chars(current.last().unwrap());
                }
                flush(&mut pages, &mut current, &mut current_chars);
                continue;
            }
            if !current.is_empty() && current_chars + item_chars > char_limit {
                flush(&mut pages, &mut current, &mut current_chars);
            }
            current.push(item);
            current_chars += item_chars;
        }
        flush(&mut pages, &mut current, &mut current_chars);
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
                    "page_token_limit": page_token_limit,
                    "created_at": Utc::now().to_rfc3339(),
                }),
            )
            .await?;

        // First page inline.
        let mut first = pages.into_iter().next().unwrap_or_default();
        first["_pagination"] = page_metadata(response_id, 1, total_pages, Some(total_items));
        Ok(first)
    }

    /// Fetch a specific page.
    pub async fn get_page(
        &self,
        seat_id: &str,
        response_id: &str,
        page: usize,
    ) -> SlcResult<Value> {
        let _ = seat_id;
        let Some(pages_val) = self
            .store
            .get_record(COLLECTION, &format!("{response_id}:pages"))
            .await?
        else {
            return Ok(
                json!({ "error": format!("Response '{response_id}' not found or already expired") }),
            );
        };
        let pages = pages_val.as_array().cloned().unwrap_or_default();
        if page < 1 || page > pages.len() {
            return Ok(
                json!({ "error": format!("Invalid page {page}; total pages {}", pages.len()) }),
            );
        }
        let mut out = pages[page - 1].clone();
        out["_pagination"] = page_metadata(response_id, page, pages.len(), None);
        // Мета-инфо о том, КОГДА и ПРИ КАКОМ лимите создан кэш: клиент,
        // получивший total_pages=1 из старого кэша, видит причину.
        if let Ok(Some(meta)) = self
            .store
            .get_record(COLLECTION, &format!("{response_id}:meta"))
            .await
        {
            if let Some(limit) = meta.get("page_token_limit") {
                out["_pagination"]["response_page_token_limit"] = limit.clone();
            }
            if let Some(ts) = meta.get("created_at") {
                out["_pagination"]["response_created_at"] = ts.clone();
            }
            let current = self.page_token_limit().await.unwrap_or(0);
            let cached = meta
                .get("page_token_limit")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            if pages.len() == 1 && cached != 0 && cached != current {
                out["_pagination"]["hint"] = json!(format!(
                    "этот ответ кэширован при лимите страницы {cached} токенов, текущий — {current}. Если клиент обрезает выхлоп: set_page_limit(<меньше>) и ПОВТОРНО вызови исходный тул (например get_document) — кэш обновится."
                ));
            }
        }
        Ok(out)
    }

    /// Delete a cached response.
    pub async fn delete_response(&self, response_id: &str) -> SlcResult<Value> {
        let pages_removed = self
            .store
            .delete_record(COLLECTION, &format!("{response_id}:pages"))
            .await?;
        self.store
            .delete_record(COLLECTION, &format!("{response_id}:meta"))
            .await?;
        if !pages_removed {
            return Ok(
                json!({ "error": format!("Response '{response_id}' not found or already expired") }),
            );
        }
        Ok(json!({ "success": true, "response_id": response_id, "deleted_pages": 1 }))
    }

    pub async fn set_page_limit(&self, tokens: usize) -> SlcResult<Value> {
        if tokens == 0 {
            return Ok(json!({ "error": "Page limit must be a positive token count" }));
        }
        // Persist under canonical + production-compat keys.
        let mut m = serde_json::Map::new();
        m.insert("page_token_limit".into(), json!(tokens));
        self.store
            .put_record(COLLECTION, "settings", &Value::Object(m.clone()))
            .await?;
        m.insert("limit_tokens".into(), json!(tokens));
        self.store
            .put_record(COLLECTION, "settings:prod", &Value::Object(m))
            .await?;
        Ok(json!({ "success": true, "page_token_limit": tokens, "limit_tokens": tokens }))
    }

    pub async fn get_settings(&self) -> SlcResult<Value> {
        let environment_override = page_token_limit_from_env();
        Ok(json!({
            "enabled": pagination_enabled_from_env(),
            "page_token_limit": self.page_token_limit().await?,
            "page_token_limit_source": if environment_override.is_some() { "environment" } else { "stored_or_default" },
            "minimum_page_token_limit": 1,
            "maximum_page_token_limit": null,
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
                let created = val
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
                if let Some(created) = created {
                    if created.with_timezone(&Utc) < cutoff {
                        let base = key.trim_end_matches(":meta");
                        if self
                            .store
                            .delete_record(COLLECTION, &format!("{base}:pages"))
                            .await?
                        {
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
    // `get_document` is a single record whose primary payload is a string.
    // Its metadata may contain non-empty arrays (`tags`, `auto_load`, or
    // `references`). Those arrays must never turn the document into a list
    // response, otherwise the large `content` string remains in the envelope
    // and every alleged page still contains the complete document.
    if obj.get("content").is_some_and(Value::is_string) {
        return None;
    }
    // Только НЕПУСТЫЕ массивы считаются списками: пустые поля объекта
    // (auto_load/references/tags у документа) иначе перехватывали бы
    // пагинацию, и одиночный объект с большим content не резался бы.
    let non_empty = |v: &Value| v.as_array().is_some_and(|a| !a.is_empty());
    for key in [
        "results",
        "content",
        "items",
        "focuses",
        "reminders",
        "notifications",
        "tasks",
        "projects",
        "docs",
        "events",
    ] {
        if let Some(v) = obj.get(key) {
            if non_empty(v) {
                return Some(key.to_string());
            }
        }
    }
    for (k, v) in obj {
        if non_empty(v) {
            return Some(k.clone());
        }
    }
    None
}

/// Build a self-driving pagination envelope. Every non-final page carries the
/// exact next MCP invocation, so even a small model can advance deterministically
/// without deriving the response id or page number itself.
fn page_metadata(
    response_id: &str,
    page: usize,
    total_pages: usize,
    total_items: Option<usize>,
) -> Value {
    let has_more = page < total_pages;
    let mut metadata = json!({
        "response_id": response_id,
        "page": page,
        "total_pages": total_pages,
        "has_more": has_more,
        "continuation_required": has_more,
        "next_page": Value::Null,
        "next_page_command": Value::Null,
    });
    if let Some(total_items) = total_items {
        metadata["total_items"] = json!(total_items);
    }
    if has_more {
        let next_page = page + 1;
        let encoded_response_id =
            serde_json::to_string(response_id).unwrap_or_else(|_| "\"\"".to_string());
        metadata["next_page"] = json!(next_page);
        metadata["next_page_command"] = json!(format!(
            "mcp__slc__get_page({{\"response_id\":{encoded_response_id},\"page\":{next_page}}})"
        ));
    }
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    fn paginator() -> (
        Paginator<std::sync::Arc<dyn StorageBackend>>,
        std::sync::Arc<dyn StorageBackend>,
    ) {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        (Paginator::new(store.clone()), store)
    }

    #[tokio::test]
    async fn paginates_a_list() {
        let (p, _) = paginator();
        let items: Vec<Value> = (0..100)
            .map(|i| json!({"i": i, "text": "x".repeat(50)}))
            .collect();
        let result = p
            .paginate_with_limit("seat_x", "resp_1", &json!({ "results": items }), 500)
            .await
            .unwrap();
        let total_pages = result["_pagination"]["total_pages"].as_i64().unwrap();
        assert!(
            total_pages > 1,
            "expected multiple pages, got {total_pages}"
        );
        assert_eq!(result["_pagination"]["page"], 1);

        // fetch a middle page
        let page2 = p.get_page("seat_x", "resp_1", 2).await.unwrap();
        assert!(page2.get("_pagination").is_some());
        assert!(page2.get("results").is_some());

        assert!(
            p.delete_response("resp_1").await.unwrap()["success"]
                .as_bool()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn single_page_no_split() {
        let (p, _) = paginator();
        let small: Vec<Value> = (0..3).map(|i| json!({"i": i})).collect();
        let result = p
            .paginate_with_limit("seat_y", "resp_s", &json!({ "items": small }), 500)
            .await
            .unwrap();
        assert_eq!(result["_pagination"]["total_pages"], 1);
    }

    #[tokio::test]
    async fn big_item_splits_by_content() {
        // Один документ больше лимита страницы — режется ПО СОДЕРЖИМОМУ
        // на части с `part: "k/n"`; все части отдаются через get_page.
        let (p, _) = paginator();
        let big = "абвгд ".repeat(4000); // ~24K символов
        let data = json!({"results": [
            {"document_id": "big", "content": big},
            {"document_id": "small", "content": "мелкий"},
        ]});
        let result = p
            .paginate_with_limit("s", "resp_big", &data, 2000) // 2000 ток × 3 = 6000 симв
            .await
            .unwrap();
        let total = result["_pagination"]["total_pages"].as_u64().unwrap();
        assert!(
            total >= 5,
            "big doc должен разбиться на части, total={total}"
        );
        // первая часть — chunk с part
        let first = result["results"][0].clone();
        assert!(
            first.get("part").is_some(),
            "часть должна быть помечена part"
        );
        // все части склеиваются в полный контент
        let mut combined = String::new();
        for page in 1..=total {
            let pg = p.get_page("s", "resp_big", page as usize).await.unwrap();
            for item in pg["results"].as_array().unwrap() {
                combined.push_str(item["content"].as_str().unwrap());
            }
        }
        assert_eq!(combined, big + "мелкий", "склейка частей = полный контент");
    }

    #[tokio::test]
    async fn get_page_reports_cached_limit_and_hint() {
        // Кэш создан при лимите 300000; текущий лимит (default 50000) другой —
        // get_page должен сообщить, при каком лимите создан кэш, и
        // подсказать, что при обрезке нужно перевызвать исходный тул.
        let (p, _) = paginator();
        // List-кэш создан при БОЛЬШОМ лимите → одна страница (сценарий
        // агента: update_context/list_documents при 300000 токенов).
        let data = json!({"results": [
            {"document_id": "a", "content": "маленький"},
            {"document_id": "b", "content": "тоже"},
        ]});
        let first = p
            .paginate_with_limit("s", "resp_hint", &data, 300_000)
            .await
            .unwrap();
        assert_eq!(first["_pagination"]["total_pages"], 1);
        let page1 = p.get_page("s", "resp_hint", 1).await.unwrap();
        let pag = &page1["_pagination"];
        assert_eq!(pag["response_page_token_limit"], 300_000);
        assert!(pag.get("response_created_at").is_some());
        // cached(300000) != current(default 50000) → hint про перевызов.
        let hint = pag.get("hint").and_then(|v| v.as_str()).unwrap_or("");
        assert!(hint.contains("set_page_limit"), "hint: {hint}");
    }

    #[tokio::test]
    async fn tiny_page_limit_does_not_panic_and_chunks() {
        // page_token_limit=1 → char_limit=3; без защиты был underflow
        // (char_limit - 200) и паника в debug / мусор в release.
        let (p, _) = paginator();
        let big = "абвгд ".repeat(1000);
        let data = json!({"document_id": "doc_x", "content": big});
        let result = p
            .paginate_with_limit("s", "resp_tiny", &data, 1)
            .await
            .unwrap();
        let total = result["_pagination"]["total_pages"].as_u64().unwrap();
        assert!(total >= 2, "должны быть части, total={total}");
        let first_len = result["content"].as_str().unwrap().chars().count();
        assert!(first_len >= 200, "часть ≥ 200 символов, got {first_len}");
    }

    #[tokio::test]
    async fn object_with_empty_arrays_still_splits_content() {
        // get_document-ответ содержит пустые массивы (auto_load/
        // references/tags) — они не должны перехватывать пагинацию.
        let (p, _) = paginator();
        let big = "абвгд ".repeat(3000);
        let data = json!({"document_id": "doc_x", "category": "custom",
                          "auto_load": [], "references": [], "tags": [],
                          "content": big});
        let result = p
            .paginate_with_limit("s", "resp_doc2", &data, 2000)
            .await
            .unwrap();
        let total = result["_pagination"]["total_pages"].as_u64().unwrap();
        assert!(total >= 3, "контент должен разбиться, total={total}");
        let mut combined = String::new();
        for page in 1..=total {
            let pg = p.get_page("s", "resp_doc2", page as usize).await.unwrap();
            combined.push_str(pg["content"].as_str().unwrap());
        }
        assert_eq!(combined, big, "склейка = полный контент");
    }

    #[tokio::test]
    async fn document_metadata_arrays_never_capture_content_pagination() {
        let (p, _) = paginator();
        let big = "абвгд ".repeat(3000);
        let data = json!({
            "document_id": "doc_with_metadata",
            "category": "custom",
            "content": big,
            "tags": ["important", "workflow"],
            "auto_load": ["base_rules"],
            "references": ["source_notes"],
        });
        let first = p
            .paginate_with_limit("s", "resp_metadata", &data, 2000)
            .await
            .unwrap();
        let total = first["_pagination"]["total_pages"].as_u64().unwrap() as usize;
        assert!(total >= 3, "document content must be split, total={total}");
        assert_eq!(first["tags"], data["tags"]);
        assert_eq!(first["auto_load"], data["auto_load"]);
        assert_eq!(first["references"], data["references"]);

        let mut combined = String::new();
        for page in 1..=total {
            let value = if page == 1 {
                first.clone()
            } else {
                p.get_page("s", "resp_metadata", page).await.unwrap()
            };
            combined.push_str(value["content"].as_str().unwrap());
            let pagination = &value["_pagination"];
            if page < total {
                assert_eq!(pagination["has_more"], true);
                assert_eq!(pagination["continuation_required"], true);
                assert_eq!(pagination["next_page"], page + 1);
                assert_eq!(
                    pagination["next_page_command"],
                    format!(
                        "mcp__slc__get_page({{\"response_id\":\"resp_metadata\",\"page\":{}}})",
                        page + 1
                    )
                );
            } else {
                assert_eq!(pagination["has_more"], false);
                assert_eq!(pagination["continuation_required"], false);
                assert!(pagination["next_page"].is_null());
                assert!(pagination["next_page_command"].is_null());
            }
        }
        assert_eq!(combined, big);
    }

    #[tokio::test]
    async fn single_object_content_splits_into_parts() {
        // get_document-подобный ответ (объект, не список) с большим
        // content — режется по содержимому; склейка = полный контент.
        let (p, _) = paginator();
        let big = "абвгд ".repeat(3000); // ~18K символов
        let data = json!({"document_id": "doc_x", "category": "custom", "content": big});
        let result = p
            .paginate_with_limit("s", "resp_doc", &data, 2000)
            .await
            .unwrap();
        let total = result["_pagination"]["total_pages"].as_u64().unwrap();
        assert!(
            total >= 3,
            "контент объекта должен разбиться, total={total}"
        );
        assert!(result.get("part").is_some(), "первая часть помечена part");
        let mut combined = String::new();
        for page in 1..=total {
            let pg = p.get_page("s", "resp_doc", page as usize).await.unwrap();
            combined.push_str(pg["content"].as_str().unwrap());
        }
        assert_eq!(combined, big, "склейка частей = полный контент документа");
    }

    #[tokio::test]
    async fn invalid_page_errors() {
        let (p, _) = paginator();
        let small: Vec<Value> = vec![json!(1), json!(2)];
        p.paginate_with_limit("seat_z", "resp_e", &json!({ "items": small }), 500)
            .await
            .unwrap();
        let err = p.get_page("seat_z", "resp_e", 99).await.unwrap();
        assert!(err.get("error").is_some());
    }

    #[tokio::test]
    async fn page_limit_accepts_any_positive_value() {
        let (p, store) = paginator();
        assert!(p.set_page_limit(0).await.unwrap()["error"].is_string());
        assert!(
            p.set_page_limit(350_000).await.unwrap()["success"]
                .as_bool()
                .unwrap()
        );
        let settings = store
            .get_record(COLLECTION, "settings")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(settings["page_token_limit"], 350_000);
    }

    #[test]
    fn default_page_is_fifty_thousand_tokens() {
        assert_eq!(DEFAULT_PAGE_TOKEN_LIMIT, 50_000);
    }
}
