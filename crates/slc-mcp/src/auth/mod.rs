//! Полный порт легаси-авторизации: users/пароли (bcrypt), JWT access+refresh
//! с ротацией, RBAC-группы, audit-log, rate-limit, Yandex OAuth2 + allowlist.
//!
//! Легаси-источник: `src/auth/*`, `src/api/routes/auth.py`, `admin.py`
//! (Python-стек slc). Хранилище — файлы vault `.slc/auth/*` (users.json,
//! oauth_rules.json, audit.jsonl); группы — предопределённые (PREDEFINED_GROUPS).
//!
//! Режимы (`SLC_AUTH`):
//! - `seat` (default) — как раньше: X-Seat-ID/cookie, пользователей нет.
//! - `full` — REST /api/* требует Bearer-JWT; сид выводится из пользователя
//!   (`user_<user_id>`), регистрация по `AUTH_ENABLED` (admin-only или открытая).

pub mod jwt;
pub mod models;
pub mod oauth;
pub mod routes;
pub mod store;

use axum::http::HeaderMap;
use models::{User, check_permission};
use std::sync::{Arc, Mutex};

/// Auth-режим веб-морды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// X-Seat-ID/cookie; пользователи и JWT не используются.
    Seat,
    /// Полный порт легаси: JWT-пользователи, RBAC, Yandex OAuth.
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

/// Счётчик неудачных попыток (rate-limit как в легаси: 5 / 60с на ключ).
#[derive(Default)]
pub struct RateLimiter {
    attempts: Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
}

impl RateLimiter {
    const MAX: usize = 5;
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

    /// Ok — можно продолжать; Err(429 message) — лимит исчерпан.
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

/// Одноразовые коды (CSRF-state OAuth + auth_code для передачи токенов),
/// TTL 120с — как легаси `oauth_pending_codes`.
pub struct PendingCodes {
    map: Mutex<std::collections::HashMap<String, PendingCode>>,
}

pub struct PendingCode {
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Для auth_code — выданные токены; для csrf_state — пусто.
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

impl PendingCodes {
    pub fn new() -> Self {
        Self { map: Mutex::new(std::collections::HashMap::new()) }
    }

    fn is_expired(created: chrono::DateTime<chrono::Utc>) -> bool {
        chrono::Utc::now().signed_duration_since(created) > chrono::Duration::seconds(120)
    }

    pub fn put(&self, code: String, pc: PendingCode) {
        let mut map = self.map.lock().unwrap();
        map.retain(|_, v| !Self::is_expired(v.created_at));
        map.insert(code, pc);
    }

    /// Найти и удалить (одноразовый), с проверкой TTL.
    pub fn take(&self, code: &str) -> Option<PendingCode> {
        let mut map = self.map.lock().unwrap();
        let pc = map.remove(code)?;
        if Self::is_expired(pc.created_at) {
            return None;
        }
        Some(pc)
    }
}

/// Состояние авторизации, живёт в AppState.
pub struct AuthState {
    pub mode: AuthMode,
    /// AUTH_ENABLED: при true регистрация только для admins.
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
            tracing::warn!("JWT_SECRET_KEY не задан — используется default-секрет (только для dev)");
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

    /// Загрузить пользователя по Bearer-токену (full-режим).
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

/// Извлечь Authorization: Bearer <token> из заголовков.
pub fn bearer_from(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Проверка прав текущего пользователя (полный порт легаси RBAC).
pub fn user_has_permission(user: &User, resource: &str, action: &str) -> bool {
    check_permission(&user.groups, resource, action)
}

/// Проверка «только superadmin может назначать admin-группы».
pub fn user_is_superadmin(user: &User) -> bool {
    user.groups.iter().any(|g| g == "superadmins")
}

pub fn user_is_admin(user: &User) -> bool {
    user.groups
        .iter()
        .any(|g| models::ADMIN_GROUP_NAMES.contains(&g.as_str()))
}

/// Сгенерировать `user_{hex}` id как в легаси (uuid4 hex16).
pub fn user_id_new() -> String {
    format!("user_{}", rand::random::<u128>() & 0xFFFF_FFFF_FFFF_FFFF)
}

/// `rule_{hex12}` как в легаси.
pub fn rule_id_new() -> String {
    format!("rule_{:012x}", rand::random::<u64>() & 0xFF_FFFF_FFFF_FFFF)
}

/// `audit_{hex16}` как в легаси.
pub fn audit_id_new() -> String {
    format!("audit_{:016x}", rand::random::<u64>())
}
