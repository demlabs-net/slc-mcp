//! Migration from the Python SLC legacy storage into the Rust engine.
//!
//! The legacy Obsidian vault layout (see `slc-mcp.legacy`):
//!
//! ```text
//! vault/<collection>/<stem>.md      — документ = markdown + YAML frontmatter
//! vault/embeddings/…                — JSON-файлы (перегенерируются, не мигрируем)
//! vault/.slc-index/…                — индексы (пересоздаются)
//! ```
//!
//! Collections: `focuses`, `ideas` (убраны из новой модели — пропускаются),
//! `knowledge_base`, `seats`, `history` / episodic, `tasks`, `projects`.
//!
//! Migration rules (user-approved):
//!
//! - **id переделываются в нормальные человекочитаемые имена**: из
//!   frontmatter `name`/`title` или первой строки тела строится slug
//!   с префиксом категории (`project_…`, `task_…`, `skill_…`, `doc_…`);
//!   пустые/мусорные имена — детерминированный fallback на старый id.
//! - **в Obsidian vault цель** — документы раскладываются по папкам
//!   категорий (`projects/`, `tasks/`, `skills/`, `knowledge/`, `history/…`,
//!   `focuses/`, `seats/`), как делает новый `ObsidianVaultStore`.
//! - **embeddings и идеи не переносятся** (перегенерация; концепция идей
//!   удалена).

use crate::error::{SlcError, SlcResult};
use crate::model::{Document, DocumentCategory, Seat, SeatStatus, UsageStats};
use crate::storage::StorageBackend;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use std::path::Path;

/// Result of a migration run.
#[derive(Debug, Default, serde::Serialize)]
pub struct MigrateReport {
    pub documents: usize,
    pub focuses: usize,
    pub seats: usize,
    pub skipped_ideas: usize,
    pub skipped_embeddings: usize,
    pub errors: Vec<String>,
}

/// Parse a legacy markdown file: YAML frontmatter (between `---` lines) +
/// body.
fn parse_legacy_md(raw: &str) -> (Map<String, Value>, String) {
    let trimmed = raw.trim_start_matches('\u{feff}');
    let body = if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let body = rest[end + 4..].trim_start_matches('\n').to_string();
            let parsed: Map<String, Value> = serde_yaml::from_str(fm).unwrap_or_default();
            return (parsed, body);
        }
        (Map::new(), trimmed.to_string())
    } else {
        (Map::new(), trimmed.to_string())
    };
    body
}

/// Human-readable id from a document's name/title/body.
fn human_id(prefix: &str, fm: &Map<String, Value>, body: &str) -> String {
    let raw = fm
        .get("name")
        .or_else(|| fm.get("title"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            body.lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim_matches('#').trim().to_string())
        })
        .unwrap_or_default();
    let slug: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let slug = slug.chars().take(48).collect::<String>();
    if slug.is_empty() {
        // Fallback: deterministic name from the old id.
        format!(
            "{prefix}legacy_{}",
            fm.get("document_id")
                .or_else(|| fm.get("focus_id"))
                .or_else(|| fm.get("task_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("doc")
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(24)
                .collect::<String>()
        )
    } else {
        format!("{prefix}{slug}")
    }
}

fn category_prefix(cat: DocumentCategory) -> &'static str {
    match cat {
        DocumentCategory::Project => "project_",
        DocumentCategory::Task => "task_",
        DocumentCategory::Skill => "skill_",
        DocumentCategory::History => "history_",
        _ => "doc_",
    }
}

/// Migrate a legacy Obsidian vault into `target` (any storage backend).
pub async fn migrate_legacy_vault(
    src: &Path,
    target: &dyn StorageBackend,
) -> SlcResult<MigrateReport> {
    let mut report = MigrateReport::default();

    // ── knowledge_base → KB documents (RAG) ──
    if let Ok(entries) = std::fs::read_dir(src.join("knowledge_base")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(e) => {
                    report.errors.push(format!("read {}: {e}", path.display()));
                    continue;
                }
            };
            let (fm, body) = parse_legacy_md(&raw);
            let cat = fm
                .get("category")
                .and_then(|v| v.as_str())
                .and_then(DocumentCategory::parse)
                .unwrap_or(DocumentCategory::Documentation);
            let old_id = fm
                .get("document_id")
                .and_then(|v| v.as_str())
                .unwrap_or("legacy_doc")
                .to_string();
            let new_id = human_id(category_prefix(cat), &fm, &body);
            let seat = fm.get("seat_id").and_then(|v| v.as_str()).map(str::to_string);
            let mut meta = crate::model::DocMeta::default();
            if let Some(t) = fm.get("doc_type").and_then(|v| v.as_str()) {
                meta.doc_type = Some(t.to_string());
            }
            let tags: Vec<String> = fm
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let mut doc = Document::new(new_id.clone(), cat, body, meta, tags, seat);
            doc.metadata.extra.insert(
                "legacy_id".into(),
                json!(old_id),
            );
            if let Err(e) = target.kb_upsert(&doc).await {
                report.errors.push(format!("kb upsert {new_id}: {e}"));
            } else {
                report.documents += 1;
            }
        }
    }

    // ── history / episodic ──
    for dir in ["history", "episodic"] {
        if let Ok(entries) = std::fs::read_dir(src.join(dir)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(&path) else { continue };
                let (fm, body) = parse_legacy_md(&raw);
                let old_id = fm
                    .get("document_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("legacy_ev")
                    .to_string();
                let new_id = human_id("history_", &fm, &body);
                let seat = fm.get("seat_id").and_then(|v| v.as_str()).map(str::to_string);
                let doc = Document::new(
                    new_id.clone(),
                    DocumentCategory::History,
                    body,
                    Default::default(),
                    vec![],
                    seat,
                );
                if let Err(e) = target.episodic_upsert(&doc).await {
                    report.errors.push(format!("episodic {new_id} ({old_id}): {e}"));
                } else {
                    report.documents += 1;
                }
            }
        }
    }

    // ── focuses → `focuses` records ──
    if let Ok(entries) = std::fs::read_dir(src.join("focuses")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let (fm, body) = parse_legacy_md(&raw);
            let seat = fm.get("seat_id").and_then(|v| v.as_str()).unwrap_or("legacy");
            let focus_id = fm
                .get("focus_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&human_id("focus_", &fm, &body))
                .to_string();
            let item = json!({
                "focus_id": focus_id,
                "seat_id": seat,
                "mind_type": "shared",
                "title": fm.get("name").or_else(|| fm.get("title")).and_then(|v| v.as_str()).unwrap_or(&body).to_string(),
                "description": body,
                "priority": fm.get("priority").and_then(|v| v.as_i64()).unwrap_or(1),
                "depends_on": [],
                "reminder_count": 0,
                "created_at": fm.get("created_at").and_then(|v| v.as_str()).and_then(|s| DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now),
                "archived": false,
            });
            if let Err(e) = target.put_record("focuses", &focus_id, &item).await {
                report.errors.push(format!("focus {focus_id}: {e}"));
            } else {
                report.focuses += 1;
            }
        }
    }

    // ── seats ──
    if let Ok(entries) = std::fs::read_dir(src.join("seats")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let (fm, _body) = parse_legacy_md(&raw);
            let seat_id = fm
                .get("seat_id")
                .and_then(|v| v.as_str())
                .unwrap_or("legacy_seat")
                .to_string();
            let now = Utc::now();
            let seat = Seat {
                seat_id,
                name: fm.get("name").and_then(|v| v.as_str()).unwrap_or("unnamed").to_string(),
                status: SeatStatus::Active,
                created_at: now,
                last_accessed: now,
                expires_at: None,
                metadata: fm
                    .get("metadata")
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default(),
                active_task_id: None,
                active_document_id: None,
                context: Map::new(),
                usage_stats: UsageStats::default(),
            };
            if let Err(e) = target.insert_seat(&seat).await {
                report.errors.push(format!("seat {}: {e}", seat.seat_id));
            } else {
                report.seats += 1;
            }
        }
    }

    // ── ideas / embeddings — skipped by design ──
    if src.join("ideas").is_dir() {
        report.skipped_ideas = std::fs::read_dir(src.join("ideas"))
            .map(|it| it.flatten().filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md")).count())
            .unwrap_or(0);
    }
    if src.join("embeddings").is_dir() {
        report.skipped_embeddings = std::fs::read_dir(src.join("embeddings"))
            .map(|it| it.flatten().count())
            .unwrap_or(0);
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::sqlite::SqliteStore;
    use std::sync::Arc;

    fn legacy_file(root: &Path, coll: &str, stem: &str, content: &str) {
        let dir = root.join(coll);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{stem}.md")), content).unwrap();
    }

    /// Build a representative legacy vault fixture (per the legacy format).
    fn fixture(root: &Path) {
        legacy_file(
            root,
            "knowledge_base",
            "project_vassista_plan",
            "---\ndocument_id: project_vassista_plan\ncategory: project\nname: Vassista Plan\nseat_id: seat_a\n---\nПлан платформы Vassista",
        );
        legacy_file(
            root,
            "knowledge_base",
            "skill_rust",
            "---\ndocument_id: skill_rust\ncategory: skill\nname: Rust Basics\n---\nКак работать с borrow checker",
        );
        legacy_file(
            root,
            "knowledge_base",
            "note_coffee",
            "---\ndocument_id: note_coffee\ncategory: documentation\n---\nПользователь любит кофе",
        );
        legacy_file(
            root,
            "history",
            "ev_abc",
            "---\ndocument_id: ev_abc\nseat_id: seat_a\n---\nработали над миграцией",
        );
        legacy_file(
            root,
            "focuses",
            "foc_123",
            "---\nfocus_id: foc_123\nseat_id: seat_a\npriority: 3\nname: Миграция SLC\n---\nДовести мигратор до ума",
        );
        legacy_file(
            root,
            "seats",
            "seat_a",
            "---\nseat_id: seat_a\nname: test\n---\n",
        );
        legacy_file(
            root,
            "ideas",
            "idea_x",
            "---\nidea_id: idea_x\n---\nстарая идея",
        );
    }

    #[tokio::test]
    async fn migrate_legacy_vault_to_new_store() {
        let root = std::env::temp_dir().join(format!("slc-legacy-fixture-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        fixture(&root);

        let store: Arc<dyn StorageBackend> = Arc::new(SqliteStore::in_memory().unwrap());
        let report = migrate_legacy_vault(&root, store.as_ref()).await.unwrap();

        // 3 KB docs + 1 episodic.
        assert_eq!(report.documents, 4, "{report:?}");
        assert_eq!(report.focuses, 1);
        assert_eq!(report.seats, 1);
        assert_eq!(report.skipped_ideas, 1, "ideas concept is gone");
        assert!(report.errors.is_empty(), "{:?}", report.errors);

        // Human-readable ids: slug from the name/title (here the legacy id
        // already equals the slug, so the same document is found by it).
        let proj = store.kb_get("project_vassista_plan").await.unwrap().expect("project doc");
        assert_eq!(proj.category, DocumentCategory::Project);
        let by_new = store
            .kb_find(
                &crate::storage::DocFilter {
                    category: Some(DocumentCategory::Project),
                    ..Default::default()
                },
                &crate::storage::DocSort::by_created(crate::storage::SortDir::Desc),
                10,
            )
            .await
            .unwrap();
        assert_eq!(by_new.len(), 1);
        assert_eq!(by_new[0].document_id, "project_vassista_plan");
        assert_eq!(by_new[0].content, "План платформы Vassista");
        assert_eq!(by_new[0].seat_id.as_deref(), Some("seat_a"));

        let skill = store.kb_get("skill_rust_basics").await.unwrap().expect("slugged skill id");
        assert_eq!(skill.category, DocumentCategory::Skill);
        assert_eq!(skill.content, "Как работать с borrow checker");

        // Legacy id preserved in metadata.
        assert_eq!(skill.metadata.extra.get("legacy_id").and_then(|v| v.as_str()), Some("skill_rust"));

        // Episodic in the episodic store, not the KB.
        assert_eq!(
            store
                .kb_find(&Default::default(), &crate::storage::DocSort::by_created(crate::storage::SortDir::Desc), 10)
                .await
                .unwrap()
                .len(),
            3
        );
        let ev = store
            .episodic_find(&Default::default(), &crate::storage::DocSort::by_created(crate::storage::SortDir::Desc), 10)
            .await
            .unwrap();
        assert_eq!(ev.len(), 1);
        // Cyrillic body has no ascii slug → deterministic fallback on the
        // legacy id.
        assert_eq!(ev[0].document_id, "history_legacy_ev_abc");

        // Focus record written.
        let focus = store.get_record("focuses", "foc_123").await.unwrap().expect("focus record");
        assert_eq!(focus["title"], "Миграция SLC");
        assert_eq!(focus["priority"], 3);

        // Seat written.
        let seat = store.get_seat("seat_a").await.unwrap().expect("seat");
        assert_eq!(seat.name, "test");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_legacy_md_handles_frontmatter_and_plain() {
        let (fm, body) = parse_legacy_md("---\ndocument_id: x\ncategory: project\n---\nТело");
        assert_eq!(fm.get("document_id").and_then(|v| v.as_str()), Some("x"));
        assert_eq!(body, "Тело");
        let (fm2, body2) = parse_legacy_md("просто текст");
        assert!(fm2.is_empty());
        assert_eq!(body2, "просто текст");
    }

    #[test]
    fn human_id_slugs_names_and_falls_back() {
        let fm: Map<String, Value> = serde_json::from_str(r#"{"name": "Vassista Plan"}"#).unwrap();
        assert_eq!(human_id("project_", &fm, ""), "project_vassista_plan");
        let empty = Map::new();
        assert_eq!(human_id("doc_", &empty, ""), "doc_legacy_doc");
    }
}
