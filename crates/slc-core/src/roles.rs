//! Роли сидов — права на уровне движка (MCP-сиды и встроенные клиенты
//! staticlib через SlcConfig).
//!
//! Задаются либо env `SLC_SEAT_ROLES` (формат: `seat_a=operator,seat_b=operator`),
//! либо программно: `SlcConfig::default().with_seat_role("seat_a", SeatRole::Operator)`
//! — единый механизм для сервера и статической библиотеки.

use std::collections::HashMap;

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
        // ghost=admin — неизвестная роль, пропущена.
        assert!(map.get("ghost").is_none());
        unsafe { std::env::remove_var("SLC_SEAT_ROLES") };
    }

    #[test]
    fn role_str_roundtrip() {
        assert!(SeatRole::parse(SeatRole::Operator.as_str()) == Some(SeatRole::Operator));
        assert!(SeatRole::parse("OPERATOR") == Some(SeatRole::Operator));
        assert!(SeatRole::parse("nope").is_none());
    }
}
