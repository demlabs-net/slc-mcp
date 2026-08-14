//! Seed core documents — system knowledge that `build_context` concatenates
//! as `base` blocks. Inserted ONCE on engine open (if missing) — never
//! overwritten, so user edits survive restarts. Category `core`, public scope.

use crate::error::SlcResult;
use crate::model::{DocMeta, Document, DocumentCategory};
use crate::storage::StorageBackend;

const CORE_DOCS: [(&str, &[&str]); 4] = [
    ("core_slc_manifest", &["manifest", "slc", "system", "core"]),
    ("core_ai_behavior", &["ai", "behavior", "rules", "core"]),
    ("core_methodology", &["methodology", "process", "workflow", "core"]),
    ("core_slc_best_practice", &["best-practice", "guide", "patterns", "core"]),
];

/// Insert the core documents if they do not exist yet. Best-effort: a failing
/// store must not block engine startup (the context simply has no base docs).
pub async fn ensure_core_documents(store: &dyn StorageBackend) -> SlcResult<usize> {
    let mut inserted = 0usize;
    for (id, tags) in CORE_DOCS {
        if store.kb_get(id).await?.is_some() {
            continue;
        }
        let content = match embedded_content(id) {
            Some(c) => c,
            None => continue,
        };
        let mut meta = DocMeta::default();
        meta.doc_type = Some("core".into());
        meta.extra.insert("seed_version".into(), serde_json::json!(2));
        let doc = Document::new(
            id.to_string(),
            DocumentCategory::Core,
            content,
            meta,
            tags.iter().map(|t| t.to_string()).collect(),
            None,
        );
        store.kb_insert(&doc).await?;
        inserted += 1;
    }
    Ok(inserted)
}

fn embedded_content(id: &str) -> Option<&'static str> {
    match id {
        "core_slc_manifest" => Some(include_str!("seed/core_slc_manifest.md")),
        "core_ai_behavior" => Some(include_str!("seed/core_ai_behavior.md")),
        "core_methodology" => Some(include_str!("seed/core_methodology.md")),
        "core_slc_best_practice" => Some(include_str!("seed/core_slc_best_practice.md")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;

    #[tokio::test]
    async fn seeds_core_documents_once() {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        let n = ensure_core_documents(store.as_ref()).await.unwrap();
        assert_eq!(n, 4, "all core docs seeded on first open");
        assert!(
            store.kb_get("core_slc_manifest").await.unwrap().is_some(),
            "manifest must exist after seeding"
        );
        let n2 = ensure_core_documents(store.as_ref()).await.unwrap();
        assert_eq!(n2, 0, "no duplicates on second open");
        let mut doc = store.kb_get("core_ai_behavior").await.unwrap().unwrap();
        doc.content = "пользовательская версия".into();
        store.kb_insert(&doc).await.unwrap();
        assert_eq!(ensure_core_documents(store.as_ref()).await.unwrap(), 0);
        let after = store.kb_get("core_ai_behavior").await.unwrap().unwrap();
        assert_eq!(after.content, "пользовательская версия");
    }
}
