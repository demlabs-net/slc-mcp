//! Full port of the legacy auth: users/passwords (bcrypt), JWT access+refresh
//! with rotation, RBAC groups, audit log, rate limiting, Yandex OAuth2 + allowlist.
//!
//! Legacy source: `src/auth/*`, `src/api/routes/auth.py`, `admin.py`
//! (the slc Python stack). Storage — vault files under `.slc/auth/*`
//! (users.json, oauth_rules.json, audit.jsonl); groups are predefined
//! (PREDEFINED_GROUPS).
//!
//! Modes (`SLC_AUTH`):
//! - `seat` (default) — as before: X-Seat-ID/cookie, no users.
//! - `full` — REST /api/* requires a Bearer JWT; the seat is derived from the
//!   user (`user_<user_id>`); registration per `AUTH_ENABLED` (admin-only or
//!   open).

pub mod jwt;
pub mod models;
pub mod oauth;
pub mod routes;
pub mod store;

use axum::http::HeaderMap;
use models::{User, check_permission};
use std::sync::{Arc, Mutex};

/// Auth mode of the web UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// X-Seat-ID/cookie; users and JWT are not used.
    Seat,
    /// Full legacy port: JWT users, RBAC, Yandex OAuth.
    Full,
}

impl AuthMode {
    pub fn from_env() -> Self {
        match std::env::var("SLC_AUTH").as_deref() {
            Ok("full") => AuthMode::Full,
            _ => AuthMode::Seat,
        }
    }
}

/// Failed attempt counter (rate limit as in legacy: 5 per 60 s per key).
#[derive(Default)]
pub struct RateLimiter {
    attempts: Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
}

impl RateLimiter {
    const MAX: usize = 5;
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

    /// Ok — may continue; Err(429 message) — the limit is exhausted.
    pub fn check(&self, key: &str) -> Result<(), String> {
        let now = std::time::Instant::now();
        let mut map = self.attempts.lock().unwrap();
        let list = map.entry(key.to_string()).or_default();
        list.retain(|t| now.duration_since(*t) < Self::WINDOW);
        if list.len() >= Self::MAX {
            return Err(format!(
                "Too many attempts. Try again in {} seconds.",
                Self::WINDOW.as_secs()
            ));
        }
        list.push(now);
        Ok(())
    }
}

/// One-time codes (OAuth CSRF state + auth_code for token handoff),
/// TTL 120 s — like legacy `oauth_pending_codes`.
pub struct PendingCodes {
    map: Mutex<std::collections::HashMap<String, PendingCode>>,
}

pub struct PendingCode {
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// For auth_code — the issued tokens; for csrf_state — empty.
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

impl PendingCodes {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn is_expired(created: chrono::DateTime<chrono::Utc>) -> bool {
        chrono::Utc::now().signed_duration_since(created) > chrono::Duration::seconds(120)
    }

    pub fn put(&self, code: String, pc: PendingCode) {
        let mut map = self.map.lock().unwrap();
        map.retain(|_, v| !Self::is_expired(v.created_at));
        map.insert(code, pc);
    }

    /// Find and remove (one-time use), with a TTL check.
    pub fn take(&self, code: &str) -> Option<PendingCode> {
        let mut map = self.map.lock().unwrap();
        let pc = map.remove(code)?;
        if Self::is_expired(pc.created_at) {
            return None;
        }
        Some(pc)
    }
}

/// Auth state; lives in AppState.
pub struct AuthState {
    pub mode: AuthMode,
    /// AUTH_ENABLED: when true, registration is for admins only.
    pub auth_enabled: bool,
    pub store: Arc<store::AuthStore>,
    pub jwt: jwt::JwtManager,
    pub rate: RateLimiter,
    pub pending: PendingCodes,
    pub oauth: oauth::YandexOAuth,
}

impl AuthState {
    pub fn from_env(vault_path: &str) -> Self {
        let mode = AuthMode::from_env();
        let auth_enabled = std::env::var("AUTH_ENABLED")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let store = Arc::new(store::AuthStore::new(vault_path));
        let jwt = jwt::JwtManager::from_env();
        if mode == AuthMode::Full && jwt.secret == jwt::DEFAULT_SECRET {
            tracing::warn!(
                "JWT_SECRET_KEY не задан — используется default-секрет (только для dev)"
            );
        }
        Self {
            mode,
            auth_enabled,
            store,
            jwt,
            rate: RateLimiter::default(),
            pending: PendingCodes::new(),
            oauth: oauth::YandexOAuth::from_env(),
        }
    }

    /// Load the user from a Bearer token (full mode).
    pub fn user_from_bearer(&self, bearer: Option<&str>) -> Option<User> {
        if self.mode != AuthMode::Full {
            return None;
        }
        let token = bearer?.trim().strip_prefix("Bearer ")?;
        let payload = self.jwt.verify_access(token).ok()?;
        let user = self.store.get_user(&payload.sub).ok()??;
        if !user.is_active {
            return None;
        }
        Some(user)
    }
}

/// Extract Authorization: Bearer <token> from the headers.
pub fn bearer_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Permission check for the current user (full legacy RBAC port).
pub fn user_has_permission(user: &User, resource: &str, action: &str) -> bool {
    check_permission(&user.groups, resource, action)
}

/// Enforces "only a superadmin may assign admin groups".
pub fn user_is_superadmin(user: &User) -> bool {
    user.groups.iter().any(|g| g == "superadmins")
}

pub fn user_is_admin(user: &User) -> bool {
    user.groups
        .iter()
        .any(|g| models::ADMIN_GROUP_NAMES.contains(&g.as_str()))
}

/// Generate a `user_{hex}` id as in legacy (uuid4 hex16).
pub fn user_id_new() -> String {
    format!("user_{}", rand::random::<u128>() & 0xFFFF_FFFF_FFFF_FFFF)
}

/// `rule_{hex12}` as in legacy.
pub fn rule_id_new() -> String {
    format!("rule_{:012x}", rand::random::<u64>() & 0xFF_FFFF_FFFF_FFFF)
}

/// `audit_{hex16}` as in legacy.
pub fn audit_id_new() -> String {
    format!("audit_{:016x}", rand::random::<u64>())
}
