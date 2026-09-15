//! Bounded "did you mean" matching for id-bearing lookups.
//!
//! An agent usually learns an id and the human name of an object in the same
//! response (`{"task_id": "...", "name": "..."}`). When it needs the object
//! again it tends to *reconstruct* the id from the name instead of copying the
//! bytes, and every reconstruction is lossy: the id slug may have been
//! truncated, the name may be Cyrillic while the id is a transliteration, and
//! keyboards/layouts swap homoglyphs. The lookup then fails with a bare
//! `document not found`, the agent retries with another guessed id, and the
//! client eventually blames the server.
//!
//! This module turns that dead end into a deterministic hint: given the
//! requested id and the ids that actually exist, it returns the small set of
//! ids that are plausibly the same object. It is a *hint*, never an automatic
//! resolution — a wrong guess must not silently retarget a write.

use crate::model::translit_fold;

/// Default number of suggestions handed back to a caller.
pub const DID_YOU_MEAN_LIMIT: usize = 3;

/// Folded ids shorter than this are not worth matching: `a` is a prefix of far
/// too many real ids to be a useful hint.
const MIN_FOLD_LEN: usize = 3;

/// A prefix relation is strong evidence (the id was truncated or gained a
/// collision suffix `_2`), so it outranks an edit-distance hit. The bound keeps
/// a short request from "matching" an unrelated long id.
const PREFIX_MAX_DIFF: usize = 64;

/// Fold a raw id into the canonical shape of a generated slug: lowercase, every
/// non-alphanumeric character is a separator, separators collapse and are
/// trimmed. Cyrillic (and Greek) visual twins of Latin letters become the Latin
/// letter, so `contacтor` compares equal to `contactor`.
pub fn fold_confusables(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        match confusable_class(lower) {
            Some(class) => out.push(class),
            None if lower.is_ascii_alphanumeric() => out.push(lower),
            None if lower.is_alphabetic() => out.push(lower),
            // Separators (`_`, `-`, `.`, space, …) collapse into one form.
            None => out.push('_'),
        }
    }
    collapse(&out)
}

/// Canonical representative of the visual class a character belongs to.
///
/// Only unambiguous twins are listed. Ambiguous letters (Cyrillic `п`/`г`)
/// are deliberately absent: a wrong fold would invent a match that does not
/// exist, which is worse than a missed hint.
fn confusable_class(c: char) -> Option<char> {
    Some(match c {
        'a' | 'а' => 'a',
        'b' | 'в' => 'b',
        'c' | 'с' => 'c',
        'e' | 'е' | 'ё' | 'э' => 'e',
        'h' | 'н' => 'h',
        'i' | 'и' | 'і' | 'ї' => 'i',
        'j' | 'ј' => 'j',
        'k' | 'к' => 'k',
        'm' | 'м' => 'm',
        'o' | 'о' | 'ο' => 'o',
        'p' | 'р' | 'ρ' => 'p',
        's' | 'ѕ' => 's',
        't' | 'т' => 't',
        'x' | 'х' | 'χ' => 'x',
        'y' | 'у' => 'y',
        _ => return None,
    })
}

/// Collapse separator runs and trim them, mirroring the id generator.
fn collapse(s: &str) -> String {
    s.split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

/// The two independent folds an id is compared under: the visual-class fold
/// (catches homoglyphs and separator drift) and the transliteration fold
/// (catches a Cyrillic name whose stored slug is Latin).
fn folds_of(raw: &str) -> [String; 2] {
    [fold_confusables(raw), translit_fold(raw)]
}

/// Tolerance for an edit-distance hit: proportional to the id length, so short
/// ids are matched tightly and long ids still tolerate a real typo.
fn tolerance(len: usize) -> usize {
    if len < 6 { 1 } else { (len / 6).clamp(1, 4) }
}

/// Bounded Levenshtein distance over chars. Returns `limit + 1` as soon as the
/// true distance is known to exceed `limit`, so a long unrelated id costs one
/// cheap row instead of a full matrix.
fn levenshtein_at_most(a: &[char], b: &[char], limit: usize) -> usize {
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current: Vec<usize> = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        let mut row_min = current[0];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
            row_min = row_min.min(current[j + 1]);
        }
        if row_min > limit {
            return limit + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()].min(limit + 1)
}

/// Score one folded pair: `(tier, distance)`, lower is a better hint.
/// Tier 0 — identical after folding (homoglyph / separator drift).
/// Tier 1 — one is a prefix of the other (truncated id, `_2` collision suffix).
/// Tier 2 — small edit distance (typo).
fn compare(a: &str, b: &str) -> Option<(u8, usize)> {
    if a == b {
        return Some((0, 0));
    }
    let (short, long) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    let short_len = short.chars().count();
    let diff = long.chars().count() - short_len;
    if short_len >= MIN_FOLD_LEN && long.starts_with(short) && diff <= PREFIX_MAX_DIFF {
        return Some((1, diff));
    }
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    let limit = tolerance(ac.len().max(bc.len()));
    let distance = levenshtein_at_most(&ac, &bc, limit);
    (distance <= limit).then_some((2, distance))
}

/// Rank `candidates` against `requested` and return at most `limit` plausible
/// ids, best first. Empty when nothing is close: a suggestion list must never
/// turn into a guess, and callers must never auto-resolve from it.
pub fn did_you_mean(requested: &str, candidates: &[String], limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let folds = folds_of(requested);
    if folds.iter().all(|fold| fold.chars().count() < MIN_FOLD_LEN) {
        return Vec::new();
    }
    let mut scored: Vec<(u8, usize, String)> = Vec::new();
    for candidate in candidates {
        if candidate == requested {
            // An exact hit is not a suggestion; the caller's lookup did not
            // miss, so it must not be redirected.
            continue;
        }
        let candidate_folds = folds_of(candidate);
        let mut best: Option<(u8, usize)> = None;
        for (left, right) in folds.iter().zip(candidate_folds.iter()) {
            if left.is_empty() || right.is_empty() {
                continue;
            }
            if let Some(score) = compare(left, right) {
                best = Some(match best {
                    Some(previous) if previous <= score => previous,
                    _ => score,
                });
            }
        }
        if let Some((tier, distance)) = best {
            scored.push((tier, distance, candidate.clone()));
        }
    }
    scored.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then(left.1.cmp(&right.1))
            .then(left.2.cmp(&right.2))
    });
    scored.dedup_by(|left, right| left.2 == right.2);
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, id)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored id of the task that started the incident: a 48-char slug
    /// truncation of a longer human name.
    const STORED: &str = "diag_disposable_contactor_roy1_probe_20260915t04";
    /// The full human name the executor saw alongside the id.
    const FULL_NAME: &str = "diag_disposable_contactor_roy1_probe_20260915t042530";

    fn long_id_case_candidates() -> Vec<String> {
        vec![
            STORED.to_string(),
            "diag_disposable_analyst_roy1_probe_20260915t04".to_string(),
            "task_example".to_string(),
        ]
    }

    #[test]
    fn confirmed_id_slug_is_the_observed_48_char_truncation() {
        assert_eq!(STORED.chars().count(), 48);
        assert_eq!(FULL_NAME.chars().count(), 52);
        assert!(FULL_NAME.starts_with(STORED));
    }

    #[test]
    fn fold_confusables_maps_homoglyphs_and_collapses_separators() {
        // Cyrillic 'а', 'о', 'с' inside an otherwise Latin id.
        assert_eq!(
            fold_confusables("diag_disposаble_cоntactоr"),
            fold_confusables("diag_disposable_contactor")
        );
        // Separator drift is irrelevant: `-`, `_`, `.` and space are one class.
        assert_eq!(
            fold_confusables("Probe-2026.09.15"),
            fold_confusables("probe_2026_09_15")
        );
        // Real Cyrillic words keep their identity (never fold to empty).
        assert_ne!(fold_confusables("диагностика"), "");
        assert_eq!(fold_confusables(""), "");
    }

    #[test]
    fn transliteration_fold_matches_a_cyrillic_name_to_its_latin_slug() {
        assert_eq!(translit_fold("Миграция БД"), "migratsiya_bd");
        assert_eq!(fold_confusables("Схема_1"), "cxema_1");
    }

    #[test]
    fn did_you_mean_matches_a_homoglyph_id() {
        // The executor retyped the id with Cyrillic lookalikes.
        let requested = "diag_disposаble_cоntactоr_roy1_probe_20260915t04";
        let hits = did_you_mean(requested, &long_id_case_candidates(), DID_YOU_MEAN_LIMIT);
        assert_eq!(hits, vec![STORED.to_string()], "{hits:?}");
    }

    #[test]
    fn did_you_mean_matches_a_truncated_prefix_in_both_directions() {
        // The caller holds the full name (longer than the stored, truncated id).
        let hits = did_you_mean(FULL_NAME, &long_id_case_candidates(), DID_YOU_MEAN_LIMIT);
        assert_eq!(hits, vec![STORED.to_string()], "{hits:?}");

        // The caller holds a short prefix of the stored id.
        let hits = did_you_mean(
            "diag_disposable_contactor",
            &long_id_case_candidates(),
            DID_YOU_MEAN_LIMIT,
        );
        assert_eq!(hits.first().map(String::as_str), Some(STORED), "{hits:?}");
    }

    #[test]
    fn did_you_mean_prefers_the_prefix_hit_over_a_nearby_typo() {
        let candidates = vec![
            "diag_disposable_analyst_roy1_probe_20260915t04".to_string(),
            STORED.to_string(),
        ];
        let hits = did_you_mean(FULL_NAME, &candidates, DID_YOU_MEAN_LIMIT);
        assert_eq!(hits.first().map(String::as_str), Some(STORED), "{hits:?}");
    }

    #[test]
    fn did_you_mean_recovers_a_collision_suffix() {
        let candidates = vec!["phase_1_plan_2".to_string(), "unrelated".to_string()];
        let hits = did_you_mean("phase_1_plan", &candidates, DID_YOU_MEAN_LIMIT);
        assert_eq!(hits, vec!["phase_1_plan_2".to_string()], "{hits:?}");
    }

    #[test]
    fn did_you_mean_returns_nothing_when_nothing_is_close() {
        let hits = did_you_mean(
            "zzz_unrelated_marker",
            &long_id_case_candidates(),
            DID_YOU_MEAN_LIMIT,
        );
        assert!(hits.is_empty(), "{hits:?}");
        // Too short to mean anything.
        assert!(did_you_mean("a", &long_id_case_candidates(), DID_YOU_MEAN_LIMIT).is_empty());
        // An exact hit is the caller's business, not a suggestion.
        assert!(did_you_mean(STORED, &long_id_case_candidates(), DID_YOU_MEAN_LIMIT).is_empty());
    }

    #[test]
    fn did_you_mean_is_bounded_and_deterministic() {
        // Eight equally close ids: the list is capped and ordered by id.
        let candidates: Vec<String> = (2..=9).map(|i| format!("phase_{i}_plan")).collect();
        let first = did_you_mean("phase_1_plan", &candidates, 2);
        assert_eq!(
            first,
            vec!["phase_2_plan".to_string(), "phase_3_plan".to_string()]
        );
        assert_eq!(first, did_you_mean("phase_1_plan", &candidates, 2));
        // A zero limit still yields one (bounded to `max(1)`), never a guess.
        assert_eq!(
            did_you_mean("phase_1_plan", &candidates, 0),
            vec!["phase_2_plan".to_string()]
        );
    }
}
