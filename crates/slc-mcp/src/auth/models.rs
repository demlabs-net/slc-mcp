//! Модели авторизации — 1:1 с легаси `src/auth/models.py`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Группы с admin-правами (легаси `ADMIN_GROUP_NAMES`).
pub const ADMIN_GROUP_NAMES: [&str; 2] = ["admins", "superadmins"];

/// Пользователь (легаси `User`, Mongo `users` → vault `.slc/auth/users.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub user_id: String,
    pub username: String,
    pub email: String,
    #[serde(default)]
    pub password_hash: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default = "default_true")]
    pub is_active: bool,
    #[serde(default)]
    pub oauth_provider: Option<String>,
    #[serde(default)]
    pub oauth_id: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_login: Option<DateTime<Utc>>,
    /// SHA-256 последнего refresh-токена (ротация, легаси
    /// `last_refresh_token_hash`).
    #[serde(default)]
    pub last_refresh_token_hash: Option<String>,
}

fn default_true() -> bool {
    true
}

impl User {
    /// Безопасное представление для API (без password_hash).
    pub fn public_json(&self, permissions: Vec<String>) -> Value {
        serde_json::json!({
            "user_id": self.user_id,
            "username": self.username,
            "email": self.email,
            "groups": self.groups,
            "permissions": permissions,
            "is_active": self.is_active,
            "created_at": self.created_at.to_rfc3339(),
            "last_login": self.last_login.map(|d| d.to_rfc3339()),
        })
    }
}

/// Правило allowlist OAuth (легаси `OAuthAccessRule`, коллекция
/// `oauth_access_rules` → vault `.slc/auth/oauth_rules.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthAccessRule {
    pub rule_id: String,
    /// login | email | domain | ya360_org | ya360_group
    #[serde(rename = "type")]
    pub rule_type: String,
    /// Нормализовано: strip().lower().
    pub value: String,
    #[serde(default = "default_users")]
    pub default_groups: Vec<String>,
    #[serde(default)]
    pub description: String,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub created_by: String,
}

fn default_users() -> Vec<String> {
    vec!["users".into()]
}

/// Запись audit (легаси `AuditLog` → vault `.slc/auth/audit.jsonl`).
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub log_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    pub timestamp: DateTime<Utc>,
}

/// Предопределённые группы (легаси `PREDEFINED_GROUPS`).
pub struct PredefinedGroup {
    pub name: &'static str,
    pub group_id: &'static str,
    pub permissions: &'static [&'static str],
}

pub const PREDEFINED_GROUPS: &[PredefinedGroup] = &[
    PredefinedGroup { name: "superadmins", group_id: "group_superadmins", permissions: &["*:*"] },
    PredefinedGroup { name: "admins", group_id: "group_admins", permissions: &["*:*"] },
    PredefinedGroup {
        name: "editors",
        group_id: "group_editors",
        permissions: &[
            "kb:read",
            "kb:write",
            "kb:delete",
            "tasks:read",
            "tasks:write",
            "tasks:delete",
            "seats:read",
        ],
    },
    PredefinedGroup {
        name: "users",
        group_id: "group_users",
        permissions: &["kb:read", "tasks:read", "seats:read:own"],
    },
    PredefinedGroup { name: "viewers", group_id: "group_viewers", permissions: &["kb:read"] },
];

/// Все пермишены группы (легаси `get_user_permissions`, union с дедупом).
pub fn permissions_for_groups(groups: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in groups {
        if let Some(pg) = PREDEFINED_GROUPS.iter().find(|pg| pg.name == g) {
            for p in pg.permissions {
                if !out.iter().any(|x| x == p) {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

/// Проверка права (легаси `Policy.check_permission`): `*:*`, `resource:*`,
/// `resource:action:scope`, `resource:action`.
pub fn check_permission(groups: &[String], resource: &str, action: &str) -> bool {
    let perms = permissions_for_groups(groups);
    let exact = format!("{resource}:{action}");
    let wildcard = format!("{resource}:*");
    perms.iter().any(|p| {
        p == "*:*" || p == &wildcard || p == &exact || p.starts_with(&format!("{exact}:"))
    })
}
