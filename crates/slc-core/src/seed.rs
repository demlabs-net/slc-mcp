//! Seed core documents — legacy parity (`dist/knowledge/core/*.json`).
//!
//! These base documents (manifest, standards, methodology…) are the system
//! knowledge that `build_context` concatenates as `base` blocks. They are
//! inserted ONCE on engine open (if missing) — never overwritten, so a user's
//! edits survive restarts. Category `core`, public scope (no seat).

use crate::error::SlcResult;
use crate::model::{DocMeta, Document, DocumentCategory};
use crate::storage::StorageBackend;

/// (document_id, tags) — content comes from the embedded JSON files.
const CORE_DOCS: [(&str, &[&str]); 7] = [
    ("core_slc_manifest", &["manifest", "slc", "system", "core"]),
    ("core_ai_behavior_correction", &["ai", "behavior", "correction", "rules", "core"]),
    ("core_development_standards", &["development", "standards", "code", "quality", "core"]),
    ("core_methodology", &["methodology", "process", "workflow", "core"]),
    ("core_standards", &["standards", "guidelines", "core"]),
    ("core_project", &["project", "config", "core"]),
    ("core_reflection_system", &["reflection", "learning", "improvement", "core"]),
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
        meta.extra.insert("seed_version".into(), serde_json::json!(1));
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

/// Raw JSON text of a core document (embedded at compile time).
fn embedded_content(id: &str) -> Option<&'static str> {
    match id {
        "core_slc_manifest" => Some(include_str!("seed/core_slc_manifest.json")),
        "core_ai_behavior_correction" => {
            Some(include_str!("seed/core_ai_behavior_correction.json"))
        }
        "core_development_standards" => {
            Some(include_str!("seed/core_development_standards.json"))
        }
        "core_methodology" => Some(include_str!("seed/core_methodology.json")),
        "core_standards" => Some(include_str!("seed/core_standards.json")),
        "core_project" => Some(include_str!("seed/core_project.json")),
        "core_reflection_system" => Some(include_str!("seed/core_reflection_system.json")),
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
        assert_eq!(n, 7, "all core docs seeded on first open");
        assert!(
            store.kb_get("core_slc_manifest").await.unwrap().is_some(),
            "manifest must exist after seeding"
        );
        // Второй вызов — идемпотентно.
        let n2 = ensure_core_documents(store.as_ref()).await.unwrap();
        assert_eq!(n2, 0, "no duplicates on second open");
        // Пользовательская правка манифеста не перезатирается.
        let mut doc = store.kb_get("core_standards").await.unwrap().unwrap();
        doc.content = "пользовательская версия".into();
        store.kb_insert(&doc).await.unwrap();
        assert_eq!(ensure_core_documents(store.as_ref()).await.unwrap(), 0);
        let after = store.kb_get("core_standards").await.unwrap().unwrap();
        assert_eq!(after.content, "пользовательская версия");
    }
}
