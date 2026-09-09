//! Seed core documents — system knowledge that `build_context` concatenates
//! as `base` blocks. Category `core`, public scope (no seat).
//!
//! Lifecycle on engine open (`ensure_core_documents`):
//! - missing → insert the current seed;
//! - present with `metadata.extra.seed_version` == SEED_VERSION → untouched
//!   (user edits survive restarts);
//! - present with an OLDER seed_version → REPLACED with the current seed
//!   (system docs, not user data);
//! - v1 leftovers not in the current set (e.g. the huge legacy JSON
//!   manifest) → soft-deleted into the graveyard.

use crate::error::SlcResult;
use crate::model::{DocMeta, Document, DocumentCategory};
use crate::storage::StorageBackend;

const CORE_DOCS: [(&str, &[&str]); 4] = [
    ("core_slc_manifest", &["manifest", "slc", "system", "core"]),
    ("core_ai_behavior", &["ai", "behavior", "rules", "core"]),
    (
        "core_methodology",
        &["methodology", "process", "workflow", "core"],
    ),
    (
        "core_slc_best_practice",
        &["best-practice", "guide", "patterns", "core"],
    ),
];

/// Current seed version. Bump = the system core documents get rewritten.
/// v3: manifest synced with the code (tokens).
/// v4: removed the byte-size output guard — the budget is token-only now.
const SEED_VERSION: i64 = 4;

/// v1 documents no longer shipped in the bundle (legacy JSON seed).
const LEGACY_V1_DOCS: [&str; 5] = [
    "core_ai_behavior_correction",
    "core_development_standards",
    "core_standards",
    "core_project",
    "core_reflection_system",
];

/// Ensure the core documents are present and current. Best-effort: a failing
/// store must not block engine startup (the context simply has no base docs).
pub async fn ensure_core_documents(store: &dyn StorageBackend) -> SlcResult<usize> {
    let mut inserted = 0usize;
    for (id, tags) in CORE_DOCS {
        let up_to_date = match store.kb_get(id).await? {
            Some(doc) => {
                doc.metadata
                    .extra
                    .get("seed_version")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    >= SEED_VERSION
            }
            None => false,
        };
        if up_to_date {
            continue;
        }
        let Some(content) = embedded_content(id) else {
            continue;
        };
        let mut meta = DocMeta::default();
        meta.doc_type = Some("core".into());
        meta.extra
            .insert("seed_version".into(), serde_json::json!(SEED_VERSION));
        let doc = Document::new(
            id.to_string(),
            DocumentCategory::Core,
            content,
            meta,
            tags.iter().map(|t| t.to_string()).collect(),
            None,
        );
        // A legacy or older seed with the same stable ID must be replaced in
        // full. `kb_insert` rejects that perfectly valid upgrade path and can
        // abort seeding of every remaining core document.
        store.kb_replace(&doc).await?;
        inserted += 1;
    }
    // Clean up legacy v1 documents (soft delete into the graveyard — recoverable).
    for id in LEGACY_V1_DOCS {
        let _ = store.kb_soft_delete(id).await;
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
        // A user edit keeps seed_version → not overwritten.
        store.kb_insert(&doc).await.unwrap();
        assert_eq!(ensure_core_documents(store.as_ref()).await.unwrap(), 0);
        let after = store.kb_get("core_ai_behavior").await.unwrap().unwrap();
        assert_eq!(after.content, "пользовательская версия");
    }

    #[tokio::test]
    async fn legacy_v1_seed_is_migrated_and_cleaned() {
        let store: std::sync::Arc<dyn StorageBackend> =
            std::sync::Arc::new(SqliteStore::in_memory().unwrap());
        // v1 manifest (huge JSON, no seed_version) + legacy document.
        let mut meta = DocMeta::default();
        meta.doc_type = Some("core".into());
        store
            .kb_insert(&Document::new(
                "core_slc_manifest".to_string(),
                DocumentCategory::Core,
                "{\"name\": \"V1MARKER легаси json-манифест, 77K символов\"}".to_string(),
                meta.clone(),
                vec![],
                None,
            ))
            .await
            .unwrap();
        store
            .kb_insert(&Document::new(
                "core_reflection_system".to_string(),
                DocumentCategory::Core,
                "легаси v1 док".to_string(),
                meta,
                vec![],
                None,
            ))
            .await
            .unwrap();

        let n = ensure_core_documents(store.as_ref()).await.unwrap();
        assert_eq!(n, 4, "v1 docs replaced by the current seed");
        let manifest = store.kb_get("core_slc_manifest").await.unwrap().unwrap();
        assert!(
            !manifest.content.contains("V1MARKER"),
            "v1 content replaced"
        );
        assert_eq!(
            manifest
                .metadata
                .extra
                .get("seed_version")
                .and_then(|v| v.as_i64()),
            Some(SEED_VERSION)
        );
        // The legacy document went to the graveyard (soft delete).
        assert!(
            store
                .kb_get("core_reflection_system")
                .await
                .unwrap()
                .is_none()
        );
        // Re-running the seeding is idempotent.
        assert_eq!(ensure_core_documents(store.as_ref()).await.unwrap(), 0);
    }
}
