//! Yandex OAuth2 — 1:1 with legacy `src/auth/oauth_yandex.py` + the
//! `_check_oauth_access` allowlist (rules > env fallback).

use super::models::OAuthAccessRule;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const YANDEX_AUTHORIZE_URL: &str = "https://oauth.yandex.ru/authorize";
const YANDEX_TOKEN_URL: &str = "https://oauth.yandex.ru/token";
const YANDEX_USERINFO_URL: &str = "https://login.yandex.ru/info";
const YANDEX_360_API_BASE: &str = "https://api360.yandex.net/directory/v1";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct YandexUserInfo {
    pub id: Option<String>,
    pub login: Option<String>,
    #[serde(rename = "default_email")]
    pub default_email: Option<String>,
    #[serde(rename = "default_avatar_id")]
    pub default_avatar_id: Option<String>,
    #[serde(rename = "first_name")]
    pub first_name: Option<String>,
    #[serde(rename = "last_name")]
    pub last_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct YandexTokenResponse {
    pub access_token: String,
}

pub struct YandexOAuth {
    pub client_id: String,
    pub client_secret: String,
    pub org_id: String,
    pub allowed_users: Vec<String>,
    pub allowed_groups: Vec<String>,
    pub y360: Option<Yandex360Client>,
}

impl YandexOAuth {
    pub fn from_env() -> Self {
        let client_id = std::env::var("YANDEX_CLIENT_ID").unwrap_or_default();
        let client_secret = std::env::var("YANDEX_CLIENT_SECRET").unwrap_or_default();
        let org_id = std::env::var("YANDEX_ORG_ID").unwrap_or_default();
        let allowed_users = csv_env("YANDEX_ALLOWED_USERS");
        let allowed_groups = csv_env("YANDEX_ALLOWED_GROUPS");
        let y360 = Yandex360Client::from_env();
        Self {
            client_id,
            client_secret,
            org_id,
            allowed_users,
            allowed_groups,
            y360,
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }

    pub fn get_authorize_url(&self, state: &str, redirect_uri: &str) -> String {
        let mut params = vec![
            ("response_type", "code".to_string()),
            ("client_id", self.client_id.clone()),
        ];
        if !redirect_uri.is_empty() {
            params.push(("redirect_uri", redirect_uri.to_string()));
        }
        if !state.is_empty() {
            params.push(("state", state.to_string()));
        }
        let query = params
            .into_iter()
            .map(|(k, v)| format!("{k}={}", urlencode(&v)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{YANDEX_AUTHORIZE_URL}?{query}")
    }

    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<YandexTokenResponse> {
        let mut params = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("client_id", self.client_id.clone()),
            ("client_secret", self.client_secret.clone()),
        ];
        if !redirect_uri.is_empty() {
            params.push(("redirect_uri", redirect_uri.to_string()));
        }
        let client = reqwest::Client::new();
        let resp = client
            .post(YANDEX_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .context("yandex token exchange")?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("yandex token exchange failed: {body}");
        }
        Ok(resp.json().await.context("yandex token parse")?)
    }

    pub async fn get_user_info(&self, access_token: &str) -> Result<YandexUserInfo> {
        let client = reqwest::Client::new();
        let resp = client
            .get(YANDEX_USERINFO_URL)
            .query(&[("format", "json")])
            .header("Authorization", format!("OAuth {access_token}"))
            .send()
            .await
            .context("yandex userinfo")?;
        if !resp.status().is_success() {
            anyhow::bail!("yandex userinfo failed: {}", resp.status());
        }
        Ok(resp.json().await.context("yandex userinfo parse")?)
    }

    /// Env-based allowlist check (legacy `is_user_allowed`) — including the
    /// quirk: `YANDEX_ALLOWED_GROUPS` is parsed but never enforced.
    pub fn is_user_allowed(&self, info: &YandexUserInfo) -> bool {
        let login = info.login.clone().unwrap_or_default();
        let email = info.default_email.clone().unwrap_or_default();
        if self.allowed_users.is_empty() && self.allowed_groups.is_empty() && self.org_id.is_empty()
        {
            return true;
        }
        if !self.allowed_users.is_empty() {
            if self
                .allowed_users
                .iter()
                .any(|u| *u == login || *u == email)
            {
                return true;
            }
        }
        if !self.org_id.is_empty()
            && self.allowed_users.is_empty()
            && self.allowed_groups.is_empty()
        {
            return true;
        }
        false
    }

    /// Allowlist by DB rules (takes precedence over env). Returns the groups
    /// of the first matching rule. Lazily calls the Y360 API for
    /// ya360_org/ya360_group.
    pub async fn check_rules(
        &self,
        rules: &[OAuthAccessRule],
        info: &YandexUserInfo,
    ) -> Option<Vec<String>> {
        if rules.is_empty() {
            return None;
        }
        let login = info.login.clone().unwrap_or_default().to_lowercase();
        let email = info
            .default_email
            .clone()
            .unwrap_or_default()
            .to_lowercase();
        let domain = email
            .rsplit_once('@')
            .map(|(_, d)| d.to_string())
            .unwrap_or_default();
        let y360 = self.y360.as_ref();
        for rule in rules {
            let value = rule.value.to_lowercase();
            let groups = if rule.default_groups.is_empty() {
                vec!["users".to_string()]
            } else {
                rule.default_groups.clone()
            };
            let matched = match rule.rule_type.as_str() {
                "login" => login == value,
                "email" => email == value,
                "domain" => domain == value,
                "ya360_org" => match y360 {
                    Some(c) if c.is_configured() => match c.is_user_in_org(&login, &email).await {
                        Ok(true) => true,
                        _ => {
                            tracing::warn!("ya360_org rule: org check failed for {login} — deny");
                            false
                        }
                    },
                    _ => {
                        tracing::warn!(
                            "ya360_org rule: Y360 не настроен (YANDEX_360_ADMIN_TOKEN + YANDEX_360_ORG_ID)"
                        );
                        false
                    }
                },
                "ya360_group" => match y360 {
                    Some(c) if c.is_configured() => match value.parse::<i64>() {
                        Ok(gid) => match c.is_user_in_group(gid, &login, &email).await {
                            Ok(true) => true,
                            _ => {
                                tracing::warn!("ya360_group({gid}) rule: check failed — deny");
                                false
                            }
                        },
                        Err(_) => {
                            tracing::warn!("ya360_group: value {value} не числовой group_id");
                            false
                        }
                    },
                    _ => {
                        tracing::warn!("ya360_group rule: Y360 не настроен — deny");
                        false
                    }
                },
                _ => false,
            };
            if matched {
                return Some(groups);
            }
        }
        None
    }
}

fn csv_env(key: &str) -> Vec<String> {
    std::env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn urlencode(s: &str) -> String {
    // Good enough for client_id/state values (alphanumeric).
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => {
                let mut out = String::new();
                for b in other.to_string().as_bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
                out
            }
        })
        .collect()
}

/// Yandex 360 Directory API (legacy `Yandex360DirectoryClient`).
pub struct Yandex360Client {
    pub admin_token: String,
    pub client_id: String,
    pub client_secret: String,
    pub org_id: String,
}

impl Yandex360Client {
    pub fn from_env() -> Option<Self> {
        let admin_token = std::env::var("YANDEX_360_ADMIN_TOKEN").unwrap_or_default();
        let client_id = std::env::var("YANDEX_360_CLIENT_ID").unwrap_or_default();
        let client_secret = std::env::var("YANDEX_360_SECRET").unwrap_or_default();
        let org_id = std::env::var("YANDEX_360_ORG_ID").unwrap_or_default();
        if admin_token.is_empty() && client_id.is_empty() && org_id.is_empty() {
            return None;
        }
        Some(Self {
            admin_token,
            client_id,
            client_secret,
            org_id,
        })
    }

    pub fn is_configured(&self) -> bool {
        !self.admin_token.is_empty() && !self.org_id.is_empty()
    }

    pub fn has_app_credentials(&self) -> bool {
        !self.client_id.is_empty() && !self.client_secret.is_empty()
    }

    pub fn auth_method(&self) -> &'static str {
        if self.is_configured() {
            "admin_token"
        } else if self.has_app_credentials() {
            "needs_auth"
        } else {
            "none"
        }
    }

    async fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
        if !self.is_configured() {
            anyhow::bail!("Y360 not configured");
        }
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{YANDEX_360_API_BASE}{path}"))
            .query(params)
            .header("Authorization", format!("OAuth {}", self.admin_token))
            .send()
            .await
            .context("y360 request")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Yandex 360 API error {status}: {body}");
        }
        Ok(resp.json().await.context("y360 parse")?)
    }

    /// Organization membership (legacy `is_user_in_org`, pagination 100/page).
    pub async fn is_user_in_org(&self, login: &str, email: &str) -> Result<bool> {
        let login_nick = login.split('@').next().unwrap_or(login).to_lowercase();
        let email_lower = email.to_lowercase();
        let mut page = 1;
        loop {
            let data = self
                .get(
                    &format!("/org/{}/users", self.org_id),
                    &[("page", &page.to_string()), ("perPage", "100")],
                )
                .await?;
            let users = data
                .get("users")
                .and_then(|u| u.as_array())
                .cloned()
                .unwrap_or_default();
            for u in &users {
                let nick = u
                    .get("nickname")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let mail = u
                    .get("email")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                if nick == login_nick || (!email_lower.is_empty() && mail == email_lower) {
                    return Ok(true);
                }
            }
            if users.len() < 100 {
                break;
            }
            page += 1;
        }
        Ok(false)
    }

    /// Group membership (legacy `is_user_in_group`).
    pub async fn is_user_in_group(&self, group_id: i64, login: &str, email: &str) -> Result<bool> {
        let login_nick = login.split('@').next().unwrap_or(login).to_lowercase();
        let email_lower = email.to_lowercase();
        let data = self
            .get(
                &format!("/org/{}/groups/{group_id}/members", self.org_id),
                &[],
            )
            .await?;
        let users = data
            .get("users")
            .and_then(|u| u.as_array())
            .cloned()
            .unwrap_or_default();
        for u in users {
            let nick = u
                .get("nickname")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let mail = u
                .get("email")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            if nick == login_nick || (!email_lower.is_empty() && mail == email_lower) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// List of organization groups (for admin status/rules).
    #[allow(dead_code)]
    pub async fn list_groups(&self) -> Result<Vec<serde_json::Value>> {
        let data = self
            .get(
                &format!("/org/{}/groups", self.org_id),
                &[("page", "1"), ("perPage", "200")],
            )
            .await?;
        Ok(data
            .get("groups")
            .and_then(|g| g.as_array())
            .cloned()
            .unwrap_or_default())
    }
}
