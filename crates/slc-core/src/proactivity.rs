//! Per-mind scoping for proactive entities (focuses / ideas / reminders).
//!
//! Legacy equivalent: `src/proactivity_scope.py` (Phase 19 P.2). Each
//! subconscious mind (`front` / `planner` / `executor` / `critic`) owns its
//! own set of proactive entities, scoped by `mind_type`. Entities created
//! outside a concrete mind context are `shared` and visible to every mind.
//!
//! Read-time normalization: legacy documents written before this field
//! existed have no `mind_type` key — treated as `shared` on read. In the Rust
//! model `MindType` always has a value (defaults to `Shared` on write), so no
//! `$exists` clause is needed; a record that reads back without a value is
//! normalized to `Shared`.

use serde::{Deserialize, Serialize};

/// The subconscious minds that scope proactive entities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MindType {
    Front,
    Planner,
    Executor,
    Critic,
    Shared,
}

/// The value persisted when no concrete mind owns the entity.
pub const DEFAULT_MIND_TYPE: MindType = MindType::Shared;

/// Sentinel for "no mind filter" — admin/system reads that want the whole
/// seat pool across every mind.
pub const ALL_MINDS: &str = "all";

impl MindType {
    pub fn as_str(self) -> &'static str {
        match self {
            MindType::Front => "front",
            MindType::Planner => "planner",
            MindType::Executor => "executor",
            MindType::Critic => "critic",
            MindType::Shared => "shared",
        }
    }

    /// Fail-fast parse — no silent fallback (mirrors `validate_mind_type`).
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "front" => Ok(MindType::Front),
            "planner" => Ok(MindType::Planner),
            "executor" => Ok(MindType::Executor),
            "critic" => Ok(MindType::Critic),
            "shared" => Ok(MindType::Shared),
            other => Err(format!(
                "invalid mind_type {other:?}; expected one of {:?}",
                sorted_mind_types()
            )),
        }
    }
}

fn sorted_mind_types() -> Vec<&'static str> {
    let mut v = ["critic", "executor", "front", "planner", "shared"].to_vec();
    v.sort_unstable();
    v
}

/// Resolve the `mind_type` to persist on write.
///
/// `None` defaults to [`DEFAULT_MIND_TYPE`] (`shared`). Any explicit value
/// must be a member of [`MindType`] (`all` is read-only).
pub fn normalize_write_mind_type(mind_type: Option<&str>) -> Result<MindType, String> {
    match mind_type {
        None => Ok(DEFAULT_MIND_TYPE),
        Some(m) if m == ALL_MINDS => Err("'all' is read-only".into()),
        Some(m) => MindType::parse(m),
    }
}

/// Does a stored entity (with *entity_mind*) pass a read scoped by *filter*?
///
/// * `None` → no filter (whole seat pool across every mind).
/// * `Shared` → only shared.
/// * a concrete mind → that mind **plus** shared.
pub fn mind_matches(entity_mind: MindType, filter: Option<MindType>) -> bool {
    match filter {
        None => true,
        Some(f) => entity_mind == f || entity_mind == MindType::Shared,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_and_invalid() {
        assert_eq!(MindType::parse("front").unwrap(), MindType::Front);
        assert_eq!(MindType::parse("shared").unwrap(), MindType::Shared);
        assert!(MindType::parse("all").is_err());
        assert!(MindType::parse("bogus").is_err());
    }

    #[test]
    fn write_normalization() {
        assert_eq!(normalize_write_mind_type(None).unwrap(), MindType::Shared);
        assert_eq!(
            normalize_write_mind_type(Some("critic")).unwrap(),
            MindType::Critic
        );
        assert!(normalize_write_mind_type(Some("all")).is_err());
        assert!(normalize_write_mind_type(Some("x")).is_err());
    }

    #[test]
    fn read_filter_matches() {
        // no filter → everything
        assert!(mind_matches(MindType::Shared, None));
        assert!(mind_matches(MindType::Front, None));
        // shared filter → only shared
        assert!(mind_matches(MindType::Shared, Some(MindType::Shared)));
        assert!(!mind_matches(MindType::Front, Some(MindType::Shared)));
        // concrete mind → that mind + shared
        assert!(mind_matches(MindType::Front, Some(MindType::Front)));
        assert!(mind_matches(MindType::Shared, Some(MindType::Front)));
        assert!(!mind_matches(MindType::Critic, Some(MindType::Front)));
    }
}
