//! Роли сидов — права на уровне движка (MCP-сиды и встроенные клиенты
//! staticlib через SlcConfig).
//!
//! Задаются либо env `SLC_SEAT_ROLES` (формат: `seat_a=operator,seat_b=operator`),
//! либо программно: `SlcConfig::default().with_seat_role("seat_a", SeatRole::Operator)`
//! — единый механизм для сервера и статической библиотеки.

use std::collections::{HashMap, HashSet};

/// Роль сида. Модель расширяемая: добавляй варианты и проверки здесь.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeatRole {
    /// Управление рабочим контекстом других сидов: может
    /// активировать/деактивировать документы, задачи, проекты и фокусы
    /// любого сида (супервизорские сиды, планировщик → исполнители).
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

/// Парсинг `SLC_SEAT_ROLES` из env: `seat_a=operator,seat_b=operator`.
/// Повторяющиеся сиды складываются; неизвестные роли игнорируются с warn.
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

/// Parse the stable workflow-principal registry.
///
/// Task ownership deliberately uses transport-neutral principals (for
/// example `manager` or `dev-junior-0`) while memory/context continues to use
/// its existing seat ids.  This registry is the only mapping between those
/// namespaces, so replacing Swarm MCP with Matrix does not rewrite task data.
/// Format: JSON object `{"manager":"dev-swarm-manager", ...}`.
pub fn parse_principal_seats_env() -> HashMap<String, String> {
    let Ok(raw) = std::env::var("SLC_PRINCIPAL_SEATS") else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, String>>(&raw) {
        Ok(mapping) => {
            if mapping
                .iter()
                .any(|(principal, seat)| principal.trim().is_empty() || seat.trim().is_empty())
            {
                tracing::warn!(
                    "SLC_PRINCIPAL_SEATS contains an empty principal or seat; ignoring it"
                );
                return HashMap::new();
            }
            if mapping.values().collect::<HashSet<_>>().len() != mapping.len() {
                tracing::warn!(
                    "SLC_PRINCIPAL_SEATS maps multiple principals to one seat; ignoring it"
                );
                return HashMap::new();
            }
            mapping
        }
        Err(error) => {
            tracing::warn!(%error, "SLC_PRINCIPAL_SEATS is not a JSON string map; ignoring it");
            HashMap::new()
        }
    }
}

/// Parse the stable workflow-principal → active policy document registry.
///
/// Format: JSON object `{"manager":"swarm_pipeline_manager_v2", ...}`.
/// The document body remains mutable SLC state; this map only ensures every
/// newly assigned task auto-loads the current policy for its assignee.
pub fn parse_principal_policy_documents_env() -> HashMap<String, String> {
    let Ok(raw) = std::env::var("SLC_PRINCIPAL_POLICY_DOCUMENTS") else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, String>>(&raw) {
        Ok(mapping)
            if mapping.iter().all(|(principal, document)| {
                !principal.trim().is_empty() && !document.trim().is_empty()
            }) =>
        {
            mapping
        }
        Ok(_) => {
            tracing::warn!(
                "SLC_PRINCIPAL_POLICY_DOCUMENTS contains an empty principal or document; ignoring it"
            );
            HashMap::new()
        }
        Err(error) => {
            tracing::warn!(%error, "SLC_PRINCIPAL_POLICY_DOCUMENTS is not a JSON string map; ignoring it");
            HashMap::new()
        }
    }
}

/// Parse task-delegation authority independently from any message transport.
///
/// Format: JSON object `{"manager":["*"],"dev-senior-0":["dev-middle-0"]}`.
/// Principals, not SLC seat ids or Swarm endpoints, are used on both sides.
pub fn parse_task_assign_acl_env() -> HashMap<String, HashSet<String>> {
    let Ok(raw) = std::env::var("SLC_TASK_ASSIGN_ACL") else {
        return HashMap::new();
    };
    match serde_json::from_str::<HashMap<String, Vec<String>>>(&raw) {
        Ok(mapping) => mapping
            .into_iter()
            .filter_map(|(actor, targets)| {
                let actor = actor.trim().to_string();
                let targets = targets
                    .into_iter()
                    .map(|target| target.trim().to_string())
                    .filter(|target| !target.is_empty())
                    .collect::<HashSet<_>>();
                (!actor.is_empty() && !targets.is_empty()).then_some((actor, targets))
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "SLC_TASK_ASSIGN_ACL is not a JSON array map; ignoring it");
            HashMap::new()
        }
    }
}

/// Principals whose model has no image input and therefore cannot publish a
/// visual acceptance verdict.  The task/report domain owns this invariant;
/// transports only deliver opaque notifications.
pub fn parse_text_only_principals_env() -> HashSet<String> {
    std::env::var("SLC_TEXT_ONLY_PRINCIPALS")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|principal| !principal.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roles() {
        unsafe {
            std::env::set_var(
                "SLC_SEAT_ROLES",
                "boss=operator, worker=operator, ghost=admin",
            )
        };
        let map = parse_roles_env();
        assert_eq!(map.get("boss").map(|r| r.len()), Some(1));
        assert!(map.get("boss").unwrap()[0] == SeatRole::Operator);
        assert_eq!(map.get("worker").map(|r| r.len()), Some(1));
        // ghost=admin — неизвестная роль, пропущена.
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
        let acl =
            parse_manage_acl("manager=developer|designer|tester,lead=developer|junior,root=*");
        assert!(acl["manager"].contains("developer"));
        assert!(acl["manager"].contains("designer"));
        assert!(!acl["manager"].contains("devops"));
        assert!(acl["lead"].contains("junior"));
        assert!(acl["root"].contains("*"));
    }

    #[test]
    fn workflow_principal_and_assignment_maps_are_transport_neutral() {
        unsafe {
            std::env::set_var(
                "SLC_PRINCIPAL_SEATS",
                r#"{"manager":"seat-manager","worker":"seat-worker"}"#,
            );
            std::env::set_var(
                "SLC_TASK_ASSIGN_ACL",
                r#"{"manager":["*"],"senior":["worker"]}"#,
            );
            std::env::set_var(
                "SLC_PRINCIPAL_POLICY_DOCUMENTS",
                r#"{"manager":"swarm_pipeline_manager_v2","worker":"swarm_pipeline_middle_v2"}"#,
            );
            std::env::set_var("SLC_TEXT_ONLY_PRINCIPALS", "worker, junior");
        }
        let principals = parse_principal_seats_env();
        assert_eq!(principals["worker"], "seat-worker");
        let acl = parse_task_assign_acl_env();
        assert!(acl["manager"].contains("*"));
        assert!(acl["senior"].contains("worker"));
        let policies = parse_principal_policy_documents_env();
        assert_eq!(policies["worker"], "swarm_pipeline_middle_v2");
        let text_only = parse_text_only_principals_env();
        assert!(text_only.contains("worker"));
        unsafe {
            std::env::set_var(
                "SLC_PRINCIPAL_SEATS",
                r#"{"manager":"shared-seat","worker":"shared-seat"}"#,
            );
        }
        assert!(parse_principal_seats_env().is_empty());
        unsafe {
            std::env::remove_var("SLC_PRINCIPAL_SEATS");
            std::env::remove_var("SLC_TASK_ASSIGN_ACL");
            std::env::remove_var("SLC_PRINCIPAL_POLICY_DOCUMENTS");
            std::env::remove_var("SLC_TEXT_ONLY_PRINCIPALS");
        }
    }
}
