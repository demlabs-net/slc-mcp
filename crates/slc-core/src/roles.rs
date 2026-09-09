//! Seat roles — engine-level permissions (MCP seats and embedded staticlib
//! clients through SlcConfig).
//!
//! Set either via env `SLC_SEAT_ROLES` (format: `seat_a=operator,seat_b=operator`),
//! or programmatically: `SlcConfig::default().with_seat_role("seat_a", SeatRole::Operator)`
//! — one mechanism for the server and the static library.

use std::collections::{HashMap, HashSet};

/// A seat's role. The model is extensible: add variants and checks here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatRole {
    /// Manages other seats' working context: can activate/deactivate
    /// documents, tasks, projects and focuses of any seat
    /// (supervisory seats, planner → executors).
    Operator,
}

impl SeatRole {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "operator" => Some(SeatRole::Operator),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SeatRole::Operator => "operator",
        }
    }

    pub const ALL: [SeatRole; 1] = [SeatRole::Operator];
}

/// Parse `SLC_SEAT_ROLES` from env: `seat_a=operator,seat_b=operator`.
/// Duplicate seats accumulate; unknown roles are ignored with a warn.
pub fn parse_roles_env() -> HashMap<String, Vec<SeatRole>> {
    let mut map: HashMap<String, Vec<SeatRole>> = HashMap::new();
    let Ok(raw) = std::env::var("SLC_SEAT_ROLES") else {
        return map;
    };
    for part in raw.split(',') {
        let Some((seat, role)) = part.split_once('=') else {
            continue;
        };
        let seat = seat.trim();
        if seat.is_empty() {
            continue;
        }
        match SeatRole::parse(role) {
            Some(role) => map.entry(seat.to_string()).or_default().push(role),
            None => {
                tracing::warn!(
                    "SLC_SEAT_ROLES: неизвестная роль {:?} для сида {:?} — игнорируется",
                    role.trim(),
                    seat
                );
            }
        }
    }
    map
}

/// Parse the cross-seat management ACL.
///
/// Format: `actor=target_a|target_b,another_operator=*`.  A role alone never
/// grants cross-seat access: the actor must also have an explicit target in
/// this ACL (or `*`).  This keeps an accidentally configured `operator` from
/// becoming a global tenant administrator.
pub fn parse_manage_acl(raw: &str) -> HashMap<String, HashSet<String>> {
    let mut map = HashMap::new();
    for rule in raw.split(',') {
        let Some((actor, targets)) = rule.split_once('=') else {
            continue;
        };
        let actor = actor.trim();
        if actor.is_empty() {
            continue;
        }
        let targets = targets
            .split('|')
            .map(str::trim)
            .filter(|target| !target.is_empty())
            .map(String::from)
            .collect::<HashSet<_>>();
        if !targets.is_empty() {
            map.insert(actor.to_string(), targets);
        }
    }
    map
}

/// Read [`parse_manage_acl`] input from `SLC_SEAT_MANAGE_ACL`.
pub fn parse_manage_acl_env() -> HashMap<String, HashSet<String>> {
    std::env::var("SLC_SEAT_MANAGE_ACL")
        .map(|raw| parse_manage_acl(&raw))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roles() {
        unsafe { std::env::set_var("SLC_SEAT_ROLES", "boss=operator, worker=operator, ghost=admin") };
        let map = parse_roles_env();
        assert_eq!(map.get("boss").map(|r| r.len()), Some(1));
        assert!(map.get("boss").unwrap()[0] == SeatRole::Operator);
        assert_eq!(map.get("worker").map(|r| r.len()), Some(1));
        // ghost=admin — unknown role, skipped.
        assert!(!map.contains_key("ghost"));
        unsafe { std::env::remove_var("SLC_SEAT_ROLES") };
    }

    #[test]
    fn role_str_roundtrip() {
        assert!(SeatRole::parse(SeatRole::Operator.as_str()) == Some(SeatRole::Operator));
        assert!(SeatRole::parse("OPERATOR") == Some(SeatRole::Operator));
        assert!(SeatRole::parse("nope").is_none());
    }

    #[test]
    fn scoped_management_acl_is_explicit() {
        let acl = parse_manage_acl(
            "manager=developer|designer|tester,lead=developer|junior,root=*",
        );
        assert!(acl["manager"].contains("developer"));
        assert!(acl["manager"].contains("designer"));
        assert!(!acl["manager"].contains("devops"));
        assert!(acl["lead"].contains("junior"));
        assert!(acl["root"].contains("*"));
    }
}
