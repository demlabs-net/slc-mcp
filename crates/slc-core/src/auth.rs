//! MCP authorization — principals, auth modes, and capability permissions.
//!
//! Legacy equivalent: `src/slc_mcp/http_transport.py` (MCPPrincipal,
//! MCPAuthMode) + `src/slc_mcp/authorization.py`. Three auth modes:
//!
//! - `LegacySeatId` — the `X-Seat-ID` header only (the previous behaviour).
//! - `BearerPlusSeat` — a bearer token (`Authorization: Bearer <token>`)
//!   **plus** `X-Seat-ID`; the token is checked against `SLC_MCP_TOKEN`
//!   (constant-time compare).
//! - `Embedded` — trusted in-process embedding; no headers needed, the caller
//!   supplies the seat explicitly.
//!
//! A principal carries a permission set; `*:*` grants everything (the legacy
//! `has_permission`).

use crate::error::{SlcError, SlcResult};

/// Well-known capability permissions (legacy `authorization.py`).
pub const PUBLIC_KNOWLEDGE_WRITE: &str = "knowledge:public:write";
pub const SETTINGS_WRITE: &str = "settings:write";

/// Everything grants every permission.
pub const ALL_PERMISSIONS: &str = "*:*";

/// Supported authentication modes (legacy `MCPAuthMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    LegacySeatId,
    BearerPlusSeat,
    Embedded,
}

/// Authenticated MCP identity + request-scoped capabilities.
#[derive(Debug, Clone)]
pub struct Principal {
    pub seat_id: String,
    pub auth_mode: AuthMode,
    pub user_id: Option<String>,
    pub permissions: Vec<String>,
}

impl Principal {
    pub fn has_permission(&self, permission: &str) -> bool {
        self.permissions.iter().any(|p| p == ALL_PERMISSIONS) || self.permissions.iter().any(|p| p == permission)
    }
}

/// Resolve the auth mode from env (`SLC_MCP_AUTH`): `legacy_seat_id`
/// (default), `bearer_plus_seat`, or `embedded`.
pub fn auth_mode_from_env() -> AuthMode {
    match std::env::var("SLC_MCP_AUTH").as_deref() {
        Ok("bearer_plus_seat") => AuthMode::BearerPlusSeat,
        Ok("embedded") => AuthMode::Embedded,
        _ => AuthMode::LegacySeatId,
    }
}

/// Default permission set for a principal built from headers.
pub fn default_permissions() -> Vec<String> {
    vec![ALL_PERMISSIONS.to_string()]
}

/// Constant-time compare of a candidate bearer token against the configured
/// `SLC_MCP_TOKEN`.
pub fn token_matches(candidate: &str) -> bool {
    let Some(expected) = std::env::var("SLC_MCP_TOKEN").ok() else {
        return false;
    };
    !expected.is_empty() && constant_time_eq(candidate.as_bytes(), expected.as_bytes())
}

/// Bind each bearer to exactly one seat when `SLC_MCP_SEAT_TOKENS` is set.
/// The JSON seat -> token map is authoritative: even an empty or malformed map
/// disables the legacy shared token, so migration can never silently fail open.
/// With the variable absent, existing shared-bearer deployments are unchanged.
fn seat_token_matches(raw: &str, seat: &str, candidate: &str) -> SlcResult<bool> {
    let tokens: std::collections::HashMap<String, String> = serde_json::from_str(raw)
        .map_err(|_| SlcError::InvalidInput("invalid SLC_MCP_SEAT_TOKENS configuration".into()))?;
    let mut unique = std::collections::HashSet::new();
    if tokens.is_empty() || tokens.iter().any(|(seat, token)| {
        seat.trim().is_empty() || seat != seat.trim() || token.len() < 32
            || token != token.trim() || !unique.insert(token)
    }) {
        return Err(SlcError::InvalidInput("invalid SLC_MCP_SEAT_TOKENS configuration".into()));
    }
    Ok(tokens.get(seat).is_some_and(|expected| {
        constant_time_eq(candidate.as_bytes(), expected.as_bytes())
    }))
}

fn bearer_matches_seat(seat: &str, candidate: &str) -> SlcResult<bool> {
    match std::env::var("SLC_MCP_SEAT_TOKENS") {
        Ok(raw) => seat_token_matches(&raw, seat, candidate),
        Err(std::env::VarError::NotPresent) => Ok(token_matches(candidate)),
        Err(_) => Err(SlcError::InvalidInput("invalid SLC_MCP_SEAT_TOKENS configuration".into())),
    }
}

/// Authenticate headers into a [`Principal`] for the configured mode.
///
/// Returns `Ok(None)` when headers are absent/insufficient (the caller turns
/// that into a 401). Returns `Err` on invalid config.
pub fn authenticate(mode: AuthMode, seat: Option<&str>, bearer: Option<&str>) -> SlcResult<Option<Principal>> {
    match mode {
        AuthMode::Embedded => {
            let Some(seat) = seat.filter(|s| !s.is_empty()) else {
                return Ok(None);
            };
            Ok(Some(Principal {
                seat_id: seat.to_string(),
                auth_mode: AuthMode::Embedded,
                user_id: None,
                permissions: default_permissions(),
            }))
        }
        AuthMode::LegacySeatId => {
            let Some(seat) = seat.filter(|s| !s.is_empty()) else {
                return Ok(None);
            };
            Ok(Some(Principal {
                seat_id: seat.to_string(),
                auth_mode: AuthMode::LegacySeatId,
                user_id: None,
                permissions: default_permissions(),
            }))
        }
        AuthMode::BearerPlusSeat => {
            let Some(seat) = seat.filter(|s| !s.is_empty()) else {
                return Ok(None);
            };
            let Some(bearer) = bearer.map(|b| b.trim()) else {
                return Ok(None);
            };
            if bearer.is_empty() || !bearer_matches_seat(seat, bearer)? {
                return Err(SlcError::InvalidInput("invalid bearer token".into()));
            }
            Ok(Some(Principal {
                seat_id: seat.to_string(),
                auth_mode: AuthMode::BearerPlusSeat,
                user_id: None,
                permissions: default_permissions(),
            }))
        }
    }
}

/// Constant-time string comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_modes_from_env_default() {
        assert_eq!(auth_mode_from_env(), AuthMode::LegacySeatId);
    }

    #[test]
    fn legacy_requires_seat() {
        assert!(authenticate(AuthMode::LegacySeatId, Some("seat_a"), None).unwrap().is_some());
        assert!(authenticate(AuthMode::LegacySeatId, None, None).unwrap().is_none());
    }

    #[test]
    fn bearer_requires_token_and_seat() {
        unsafe { std::env::set_var("SLC_MCP_TOKEN", "sekrit") };
        let ok = authenticate(AuthMode::BearerPlusSeat, Some("s"), Some("sekrit")).unwrap();
        assert!(ok.is_some());
        assert!(authenticate(AuthMode::BearerPlusSeat, Some("s"), Some("wrong")).is_err());
        assert!(authenticate(AuthMode::BearerPlusSeat, None, Some("sekrit")).unwrap().is_none());
        let bound = "b".repeat(64);
        unsafe { std::env::set_var("SLC_MCP_SEAT_TOKENS", serde_json::json!({"s": bound}).to_string()) };
        let principal = authenticate(AuthMode::BearerPlusSeat, Some("s"), Some(&bound)).unwrap().unwrap();
        assert_eq!(principal.seat_id, "s");
        assert!(authenticate(AuthMode::BearerPlusSeat, Some("operator"), Some(&bound)).is_err());
        assert!(authenticate(AuthMode::BearerPlusSeat, Some("s"), Some("sekrit")).is_err());
        assert!(authenticate(AuthMode::BearerPlusSeat, Some("s"), None).unwrap().is_none());
        unsafe { std::env::set_var("SLC_MCP_SEAT_TOKENS", "{}"); }
        assert!(authenticate(AuthMode::BearerPlusSeat, Some("s"), Some("sekrit")).is_err());
        unsafe {
            std::env::remove_var("SLC_MCP_SEAT_TOKENS");
            std::env::remove_var("SLC_MCP_TOKEN");
        }
    }

    #[test]
    fn bound_tokens_reject_impersonation_and_invalid_configuration() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let raw = serde_json::json!({"worker": a, "operator": b}).to_string();
        assert!(seat_token_matches(&raw, "worker", &a).unwrap());
        assert!(seat_token_matches(&raw, "operator", &b).unwrap());
        assert!(!seat_token_matches(&raw, "operator", &a).unwrap());
        assert!(!seat_token_matches(&raw, "worker", &b).unwrap());
        assert!(!seat_token_matches(&raw, "unknown", &a).unwrap());
        assert!(!seat_token_matches(&raw, "worker", "").unwrap());
        assert!(!seat_token_matches(&raw, "worker", "old-shared-token").unwrap());
        for invalid in ["", "{}", "not-json", "{\"s\":\"short\"}"] {
            assert!(seat_token_matches(invalid, "worker", &a).is_err());
        }
        let duplicate = serde_json::json!({"worker": a, "operator": a}).to_string();
        assert!(seat_token_matches(&duplicate, "worker", &a).is_err());
        assert!(authenticate(AuthMode::BearerPlusSeat, Some("worker"), None).unwrap().is_none());
    }

    #[test]
    fn embedded_requires_seat() {
        assert!(authenticate(AuthMode::Embedded, Some("s"), None).unwrap().is_some());
        assert!(authenticate(AuthMode::Embedded, None, None).unwrap().is_none());
    }

    #[test]
    fn permission_checks() {
        let p = Principal { seat_id: "s".into(), auth_mode: AuthMode::Embedded, user_id: None, permissions: vec!["settings:write".into()] };
        assert!(p.has_permission(SETTINGS_WRITE));
        assert!(!p.has_permission(PUBLIC_KNOWLEDGE_WRITE));
        let superuser = Principal { seat_id: "s".into(), auth_mode: AuthMode::Embedded, user_id: None, permissions: vec![ALL_PERMISSIONS.into()] };
        assert!(superuser.has_permission(PUBLIC_KNOWLEDGE_WRITE));
    }
}
