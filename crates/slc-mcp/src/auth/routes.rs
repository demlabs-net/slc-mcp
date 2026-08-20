//! Эндпоинты авторизации — 1:1 с легаси `src/api/routes/auth.py` +
//! auth-часть `src/api/routes/admin.py`. В seat-режиме (SLC_AUTH=seat)
//! auth-роуты отвечают 404, кроме `/me` (SPA определяет режим).

use super::{
    AuthMode, audit_id_new, bearer_from, rule_id_new, user_has_permission, user_id_new,
    user_is_admin, user_is_superadmin, PendingCode,
};
use super::models::{
    ADMIN_GROUP_NAMES, AuditEntry, OAuthAccessRule, User, permissions_for_groups,
};
use super::store::AuthStore;
use crate::server::AppState;
use crate::webui::api::ApiError;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

type AuthResult = Result<Json<Value>, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn unauthorized(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, msg.into())
}
fn forbidden(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::FORBIDDEN, msg.into())
}
fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}
fn internal(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg.into())
}

/// Auth недоступен в seat-режиме.
fn auth_disabled() -> ApiError {
    ApiError(
        StatusCode::NOT_FOUND,
        "auth is disabled (SLC_AUTH=seat — включи SLC_AUTH=full)".to_string(),
    )
}

// ── helpers ────────────────────────────────────────────────────────────

/// IP для audit/rate-limit (X-Forwarded-For за nginx).
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Текущий пользователь (full-режим) из Bearer-заголовка.
fn current_user(state: &AppState, headers: &HeaderMap) -> Result<User, ApiError> {
    if state.auth.mode != AuthMode::Full {
        return Err(auth_disabled());
    }
    let user = state.auth.user_from_bearer(bearer_from(headers).as_deref());
    match user {
        Some(u) => Ok(u),
        None => Err(unauthorized("Invalid authentication credentials")),
    }
}

/// Токены + пользователь (легаси LoginResponse).
fn login_response(state: &AppState, user: &User) -> Result<Json<Value>, ApiError> {
    let access = state
        .auth
        .jwt
        .create_access_token(&user.user_id, &user.username, &user.groups)
        .map_err(|e| internal(e))?;
    let refresh = state
        .auth
        .jwt
        .create_refresh_token(&user.user_id)
        .map_err(|e| internal(e))?;
    // Легаси LoginResponse.user — только базовые поля.
    let u = json!({
        "user_id": user.user_id,
        "username": user.username,
        "email": user.email,
        "groups": user.groups,
    });
    Ok(Json(json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "bearer",
        "user": u,
    })))
}

/// Одноразовый код (32 байта hex).
fn random_code() -> String {
    let b: [u8; 32] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Callback URI Яндекса с учётом прокси (легаси `_yandex_callback_uri`).
fn yandex_callback_uri(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("https");
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("host").and_then(|v| v.to_str().ok()))
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_default();
    format!("{scheme}://{host}/api/auth/oauth/yandex/callback")
}

fn frontend_base(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("https");
    let host = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| headers.get("host").and_then(|v| v.to_str().ok()))
        .map(|h| h.split(':').next().unwrap_or(h).to_string())
        .unwrap_or_default();
    format!("{scheme}://{host}")
}

// ── /api/auth/* ────────────────────────────────────────────────────────

pub async fn register(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let state = state.as_ref();
    if state.auth.mode != AuthMode::Full {
        return auth_disabled().into_response();
    }
    if let Err(msg) = state
        .auth
        .rate
        .check(&format!("register:{}", client_ip(&headers)))
    {
        return ApiError(StatusCode::TOO_MANY_REQUESTS, msg).into_response();
    }
    let username = body.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let email = body.get("email").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let requested: Vec<String> = body
        .get("groups")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();

    if !(3..=50).contains(&username.chars().count()) {
        return bad("username must be 3-50 chars").into_response();
    }
    if email.is_empty() || !email.contains('@') {
        return bad("email required").into_response();
    }
    if !(8..=128).contains(&password.chars().count()) {
        return bad("password must be 8-128 chars").into_response();
    }
    if requested.len() > 20 {
        return bad("too many groups (max 20)").into_response();
    }

    // AUTH_ENABLED=true → только admin (легаси auth.py:177-198).
    let current = if state.auth.auth_enabled {
        let user = match current_user(state, &headers) {
            Ok(u) => u,
            Err(e) => return e.into_response(),
        };
        if !user_has_permission(&user, "admin", "write") {
            let _ = state.auth.store.append_audit(&AuditEntry {
                log_id: audit_id_new(),
                user_id: Some(user.user_id.clone()),
                username: Some(user.username.clone()),
                action: "permission_denied".into(),
                resource: Some("auth:register".into()),
                success: false,
                ip_address: Some(client_ip(&headers)),
                user_agent: None,
                metadata: Default::default(),
                timestamp: chrono::Utc::now(),
            });
            return forbidden("Only admins can register new users").into_response();
        }
        Some(user)
    } else {
        None
    };

    // Self-escalation: admin-группы назначает только superadmin
    // (легаси auth.py:202-210).
    if state.auth.auth_enabled
        && current.is_some()
        && requested.iter().any(|g| ADMIN_GROUP_NAMES.contains(&g.as_str()))
        && !user_is_superadmin(current.as_ref().unwrap())
    {
        return forbidden("Only superadmins can assign admin/superadmin groups").into_response();
    }

    let store: &AuthStore = state.auth.store.as_ref();
    if store.find_by_username(username).is_some() {
        return bad("Username already exists").into_response();
    }
    if store.find_by_email(email).is_some() {
        return bad("Email already exists").into_response();
    }

    let groups = if requested.is_empty() { vec!["users".into()] } else { requested };
    let now = chrono::Utc::now();
    let user = User {
        user_id: user_id_new(),
        username: username.into(),
        email: email.into(),
        password_hash: bcrypt::hash(password, bcrypt::DEFAULT_COST).unwrap_or_default(),
        groups,
        is_active: true,
        oauth_provider: None,
        oauth_id: None,
        avatar_url: None,
        created_at: now,
        updated_at: now,
        last_login: None,
        last_refresh_token_hash: None,
    };
    if let Err(e) = store.insert_user(&user) {
        return internal(e.to_string()).into_response();
    }
    let _ = state.auth.store.append_audit(&AuditEntry {
        log_id: audit_id_new(),
        user_id: Some(user.user_id.clone()),
        username: Some(user.username.clone()),
        action: "user_registered".into(),
        resource: None,
        success: true,
        ip_address: Some(client_ip(&headers)),
        user_agent: None,
        metadata: json!({
            "created_by": current.as_ref().map(|c| c.username.clone()).unwrap_or_else(|| "self".into())
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
        timestamp: chrono::Utc::now(),
    });
    match login_response(state, &user) {
        Ok(resp) => (StatusCode::CREATED, resp).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let state = state.as_ref();
    if state.auth.mode != AuthMode::Full {
        return auth_disabled().into_response();
    }
    if let Err(msg) = state
        .auth
        .rate
        .check(&format!("login:{}", client_ip(&headers)))
    {
        return ApiError(StatusCode::TOO_MANY_REQUESTS, msg).into_response();
    }
    let username = body.get("username").and_then(|v| v.as_str()).unwrap_or("");
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    let ip = client_ip(&headers);

    let Some(user) = state.auth.store.find_by_username(username) else {
        let _ = state.auth.store.append_audit(&AuditEntry {
            log_id: audit_id_new(),
            user_id: None,
            username: Some(username.into()),
            action: "failed_login".into(),
            resource: None,
            success: false,
            ip_address: Some(ip),
            user_agent: None,
            metadata: Default::default(),
            timestamp: chrono::Utc::now(),
        });
        return unauthorized("Incorrect username or password").into_response();
    };
    let password_ok = !user.password_hash.is_empty()
        && bcrypt::verify(password, &user.password_hash).unwrap_or(false);
    if !password_ok {
        let _ = state.auth.store.append_audit(&AuditEntry {
            log_id: audit_id_new(),
            user_id: Some(user.user_id.clone()),
            username: Some(user.username.clone()),
            action: "failed_login".into(),
            resource: None,
            success: false,
            ip_address: Some(ip),
            user_agent: None,
            metadata: Default::default(),
            timestamp: chrono::Utc::now(),
        });
        return unauthorized("Incorrect username or password").into_response();
    }
    if !user.is_active {
        return forbidden("User account is disabled").into_response();
    }

    let mut user = user;
    user.last_login = Some(chrono::Utc::now());
    user.updated_at = chrono::Utc::now();
    if let Err(e) = state.auth.store.update_user(&user) {
        return internal(e.to_string()).into_response();
    }
    let _ = state.auth.store.append_audit(&AuditEntry {
        log_id: audit_id_new(),
        user_id: Some(user.user_id.clone()),
        username: Some(user.username.clone()),
        action: "login".into(),
        resource: None,
        success: true,
        ip_address: Some(ip),
        user_agent: None,
        metadata: Default::default(),
        timestamp: chrono::Utc::now(),
    });
    match login_response(state, &user) {
        Ok(resp) => resp.into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn refresh(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> AuthResult {
    if state.auth.mode != AuthMode::Full {
        return Err(auth_disabled());
    }
    let token = body.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("");
    let user_id = match state.auth.jwt.verify_refresh(token) {
        Ok(id) => id,
        Err(_) => return Err(unauthorized("Invalid refresh token")),
    };
    let Some(mut user) = state.auth.store.get_user(&user_id).map_err(|e| internal(e.to_string()))? else {
        return Err(unauthorized("User not found"));
    };
    if !user.is_active {
        return Err(forbidden("User account is disabled"));
    }

    // Ротация с детектом replay (легаси auth.py:393-406).
    let token_hash = sha256_hex(token);
    if user.last_refresh_token_hash.as_deref() == Some(&token_hash) {
        user.last_refresh_token_hash = None;
        let _ = state.auth.store.update_user(&user);
        return Err(unauthorized("Refresh token reuse detected. Please login again."));
    }
    let new_refresh = state
        .auth
        .jwt
        .create_refresh_token(&user.user_id)
        .map_err(|e| internal(e))?;
    user.last_refresh_token_hash = Some(sha256_hex(&new_refresh));
    user.updated_at = chrono::Utc::now();
    state.auth.store.update_user(&user).map_err(|e| internal(e.to_string()))?;
    login_response(&state, &user)
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AuthResult {
    let user = current_user(state.as_ref(), &headers)?;
    let _ = state.auth.store.append_audit(&AuditEntry {
        log_id: audit_id_new(),
        user_id: Some(user.user_id.clone()),
        username: Some(user.username.clone()),
        action: "logout".into(),
        resource: None,
        success: true,
        ip_address: Some(client_ip(&headers)),
        user_agent: None,
        metadata: Default::default(),
        timestamp: chrono::Utc::now(),
    });
    Ok(Json(json!({"message": "Logged out successfully"})))
}

/// /me — в full-режиме профиль пользователя; в seat-режиме — сид
/// (SPA по этому ответу понимает, показывать ли логин).
pub async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if state.auth.mode != AuthMode::Full {
        let seat = match crate::webui::api::resolve_seat(state.as_ref(), &headers).await {
            Ok((seat, _)) => seat,
            Err(e) => return e.into_response(),
        };
        return Json(json!({
            "authenticated": true,
            "seat_id": seat,
            "auth_mode": "seat",
        }))
        .into_response();
    }
    match current_user(state.as_ref(), &headers) {
        Ok(user) => Json(json!({
            "authenticated": true,
            "auth_mode": "full",
            "user": user.public_json(permissions_for_groups(&user.groups)),
        }))
        .into_response(),
        Err(e) => e.into_response(),
    }
}

// ── Yandex OAuth ───────────────────────────────────────────────────────

fn oauth_not_configured() -> ApiError {
    ApiError(
        StatusCode::NOT_IMPLEMENTED,
        "Yandex OAuth is not configured. Set YANDEX_CLIENT_ID and YANDEX_CLIENT_SECRET".into(),
    )
}

pub async fn oauth_callback_uri(headers: HeaderMap) -> AuthResult {
    let uri = yandex_callback_uri(&headers);
    Ok(Json(json!({
        "callback_uri": uri,
        "hint": "Укажите это значение в настройках приложения: https://oauth.yandex.ru",
    })))
}

pub async fn oauth_yandex_redirect(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if state.auth.mode != AuthMode::Full {
        return auth_disabled().into_response();
    }
    if !state.auth.oauth.is_configured() {
        return oauth_not_configured().into_response();
    }
    let csrf = random_code();
    state.auth.pending.put(
        csrf.clone(),
        PendingCode { created_at: chrono::Utc::now(), access_token: None, refresh_token: None },
    );
    let uri = yandex_callback_uri(&headers);
    let url = state.auth.oauth.get_authorize_url(&csrf, &uri);
    Redirect::to(&url).into_response()
}

/// Allowlist: правила БД > env (легаси `_check_oauth_access`).
async fn oauth_allowlist(
    state: &AppState,
    info: &super::oauth::YandexUserInfo,
) -> Option<Vec<String>> {
    let rules = state.auth.store.list_rules();
    if !rules.is_empty() {
        return state.auth.oauth.check_rules(&rules, info).await;
    }
    if state.auth.oauth.is_user_allowed(info) {
        return Some(vec!["users".into()]);
    }
    None
}

/// Ошибка OAuth-логина: `NotAllowed(info)` — юзер не в allowlist
/// (нужен для hint в callback-редиректе).
enum OauthError {
    Api(ApiError),
    NotAllowed(super::oauth::YandexUserInfo),
}

impl From<ApiError> for OauthError {
    fn from(e: ApiError) -> Self {
        OauthError::Api(e)
    }
}

async fn oauth_login(
    state: &AppState,
    headers: &HeaderMap,
    code: &str,
) -> Result<User, OauthError> {
    let redirect_uri = yandex_callback_uri(headers);
    let token_data = state
        .auth
        .oauth
        .exchange_code(code, &redirect_uri)
        .await
        .map_err(|e| bad(format!("Failed to exchange OAuth code: {e}")))?;
    let info = state
        .auth
        .oauth
        .get_user_info(&token_data.access_token)
        .await
        .map_err(|e| bad(format!("Failed to get user info from Yandex: {e}")))?;

    let login = info.login.clone().unwrap_or_default();
    let email = info.default_email.clone().unwrap_or_else(|| format!("{login}@yandex.ru"));
    let ip = client_ip(headers);

    let Some(groups) = oauth_allowlist(state, &info).await else {
        let _ = state.auth.store.append_audit(&AuditEntry {
            log_id: audit_id_new(),
            user_id: None,
            username: Some(login.clone()),
            action: "oauth_login_denied".into(),
            resource: None,
            success: false,
            ip_address: Some(ip),
            user_agent: None,
            metadata: json!({
                "provider": "yandex",
                "reason": "not_in_allowlist",
                "email": email.clone(),
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
            timestamp: chrono::Utc::now(),
        });
        return Err(OauthError::NotAllowed(info));
    };

    let yandex_id = info.id.clone().unwrap_or_default();
    let avatar_url = info
        .default_avatar_id
        .as_ref()
        .map(|a| format!("https://avatars.yandex.net/get-yapic/{a}/islands-200"));
    let now = chrono::Utc::now();
    let user = match state.auth.store.find_by_oauth("yandex", &yandex_id) {
        Some(mut u) => {
            u.last_login = Some(now);
            u.updated_at = now;
            if avatar_url.is_some() {
                u.avatar_url = avatar_url;
            }
            state.auth.store.update_user(&u).map_err(|e| internal(e.to_string()))?;
            u
        }
        None => {
            let u = User {
                user_id: user_id_new(),
                username: login.clone(),
                email,
                password_hash: String::new(),
                groups,
                is_active: true,
                oauth_provider: Some("yandex".into()),
                oauth_id: Some(yandex_id),
                avatar_url,
                created_at: now,
                updated_at: now,
                last_login: Some(now),
                last_refresh_token_hash: None,
            };
            state.auth.store.insert_user(&u).map_err(|e| internal(e.to_string()))?;
            u
        }
    };
    let _ = state.auth.store.append_audit(&AuditEntry {
        log_id: audit_id_new(),
        user_id: Some(user.user_id.clone()),
        username: Some(user.username.clone()),
        action: "login".into(),
        resource: None,
        success: true,
        ip_address: Some(ip),
        user_agent: None,
        metadata: json!({"provider": "yandex"}).as_object().cloned().unwrap_or_default(),
        timestamp: chrono::Utc::now(),
    });
    Ok(user)
}

/// GET /api/auth/oauth/yandex/callback?code&state — код → токены под
/// одноразовым auth_code → редирект на фронт (легаси auth.py:627-738).
pub async fn oauth_yandex_callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if state.auth.mode != AuthMode::Full {
        return auth_disabled().into_response();
    }
    let code = q.get("code").cloned().unwrap_or_default();
    let st = q.get("state").cloned().unwrap_or_default();
    if st.is_empty() {
        return bad("Missing OAuth state parameter (CSRF)").into_response();
    }
    if state.auth.pending.take(&st).is_none() {
        return bad("Invalid or expired OAuth state (CSRF)").into_response();
    }
    if code.is_empty() {
        return bad("Missing OAuth code").into_response();
    }
    let user = match oauth_login(state.as_ref(), &headers, &code).await {
        Ok(u) => u,
        Err(OauthError::Api(e)) => return e.into_response(),
        Err(OauthError::NotAllowed(info)) => {
            // Как легаси: deny → редирект на логин с hint (login/email).
            let hint = info
                .login
                .or(info.default_email)
                .unwrap_or_default();
            return Redirect::to(&format!(
                "{}/#/login?error=access_denied&hint={}",
                frontend_base(&headers),
                urlencode(&hint)
            ))
            .into_response();
        }
    };
    // Токены — только через одноразовый код (никогда в URL).
    let access = state.auth.jwt.create_access_token(&user.user_id, &user.username, &user.groups);
    let refresh = state.auth.jwt.create_refresh_token(&user.user_id);
    match (access, refresh) {
        (Ok(a), Ok(r)) => {
            let auth_code = random_code();
            state.auth.pending.put(
                auth_code.clone(),
                PendingCode {
                    created_at: chrono::Utc::now(),
                    access_token: Some(a),
                    refresh_token: Some(r),
                },
            );
            Redirect::to(&format!("{}/?auth_code={auth_code}", frontend_base(&headers)))
                .into_response()
        }
        _ => ApiError(StatusCode::INTERNAL_SERVER_ERROR, "token creation failed".into()).into_response(),
    }
}

fn urlencode(s: &str) -> String {
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

/// POST /api/auth/oauth/yandex {code} — SPA-поток, сразу LoginResponse
/// (легаси auth.py:769-846).
pub async fn oauth_yandex_code(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if state.auth.mode != AuthMode::Full {
        return auth_disabled().into_response();
    }
    let code = body.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if code.is_empty() {
        return bad("Missing OAuth code").into_response();
    }
    match oauth_login(state.as_ref(), &headers, code).await {
        Ok(user) => match login_response(state.as_ref(), &user) {
            Ok(resp) => resp.into_response(),
            Err(e) => e.into_response(),
        },
        Err(OauthError::Api(e)) => e.into_response(),
        Err(OauthError::NotAllowed(_)) => forbidden("User not in allowlist").into_response(),
    }
}

/// POST /api/auth/exchange {code} — одноразовый код → токены (легаси
/// auth.py:741-766).
pub async fn exchange(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> AuthResult {
    if state.auth.mode != AuthMode::Full {
        return Err(auth_disabled());
    }
    let code = body.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let Some(pc) = state.auth.pending.take(code) else {
        return Err(bad("Invalid or expired code"));
    };
    match (pc.access_token, pc.refresh_token) {
        (Some(access_token), Some(refresh_token)) => Ok(Json(json!({
            "access_token": access_token,
            "refresh_token": refresh_token,
            "token_type": "bearer",
        }))),
        _ => Err(bad("Invalid or expired code")),
    }
}

// ── admin: users / groups (легаси auth.py:849-934) ─────────────────────

fn admin_required(state: &AppState, headers: &HeaderMap) -> Result<User, ApiError> {
    let user = current_user(state, headers)?;
    if !user_is_admin(&user) {
        return Err(forbidden("Admin access required"));
    }
    Ok(user)
}

pub async fn list_users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AuthResult {
    admin_required(state.as_ref(), &headers)?;
    let users: Vec<Value> = state
        .auth
        .store
        .list_users()
        .into_iter()
        .take(500)
        .map(|u| {
            json!({
                "user_id": u.user_id,
                "username": u.username,
                "email": u.email,
                "groups": u.groups,
                "is_active": u.is_active,
                "oauth_provider": u.oauth_provider,
                "created_at": u.created_at.to_rfc3339(),
                "last_login": u.last_login.map(|d| d.to_rfc3339()),
            })
        })
        .collect();
    Ok(Json(json!({ "users": users })))
}

/// PUT /api/auth/users/{user_id}/groups — тело: голый JSON-массив строк
/// (легаси-кварк).
pub async fn update_user_groups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Json(body): Json<Value>,
) -> AuthResult {
    let actor = current_user(state.as_ref(), &headers)?;
    let groups: Vec<String> = body
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if groups.iter().any(|g| ADMIN_GROUP_NAMES.contains(&g.as_str())) {
        if !user_is_superadmin(&actor) {
            return Err(forbidden("Only superadmins can assign admin groups"));
        }
    } else if !user_has_permission(&actor, "admin", "write") {
        return Err(forbidden("Admin access required"));
    }
    let Some(mut user) = state.auth.store.get_user(&user_id).map_err(|e| internal(e.to_string()))? else {
        return Err(not_found("User not found"));
    };
    user.groups = groups;
    user.updated_at = chrono::Utc::now();
    state.auth.store.update_user(&user).map_err(|e| internal(e.to_string()))?;
    Ok(Json(json!({ "success": true })))
}

/// PUT /api/auth/users/{user_id}/active?is_active= — query-параметр
/// (легаси-кварк).
pub async fn toggle_user_active(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> AuthResult {
    let actor = current_user(state.as_ref(), &headers)?;
    if !user_has_permission(&actor, "admin", "write") {
        return Err(forbidden("Admin access required"));
    }
    let is_active = q.get("is_active").map(|v| v == "true").unwrap_or(false);
    let Some(mut user) = state.auth.store.get_user(&user_id).map_err(|e| internal(e.to_string()))? else {
        return Err(not_found("User not found"));
    };
    user.is_active = is_active;
    user.updated_at = chrono::Utc::now();
    state.auth.store.update_user(&user).map_err(|e| internal(e.to_string()))?;
    Ok(Json(json!({ "success": true })))
}

pub async fn list_groups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AuthResult {
    admin_required(state.as_ref(), &headers)?;
    let groups: Vec<Value> = super::models::PREDEFINED_GROUPS
        .iter()
        .map(|g| {
            json!({
                "name": g.name,
                "group_id": g.group_id,
                "permissions": g.permissions,
                "is_system": true,
            })
        })
        .collect();
    Ok(Json(json!({ "groups": groups })))
}

// ── admin: oauth rules (легаси admin.py) ───────────────────────────────

fn oauth_rule_types() -> [&'static str; 5] {
    ["login", "email", "domain", "ya360_org", "ya360_group"]
}

pub async fn oauth_rules_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AuthResult {
    // Легаси: admin-роуты требуют только аутентификацию (кварк 1:1).
    current_user(state.as_ref(), &headers)?;
    let mut rules = state.auth.store.list_rules();
    rules.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let rules: Vec<Value> = rules.into_iter().take(500).map(|r| json!(r)).collect();
    Ok(Json(json!({ "rules": rules, "total": rules.len() })))
}

pub async fn oauth_rule_create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if state.auth.mode != AuthMode::Full {
        return auth_disabled().into_response();
    }
    let _actor = match current_user(state.as_ref(), &headers) {
        Ok(u) => u,
        Err(e) => return e.into_response(),
    };
    let rule_type = body.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let value = body.get("value").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
    let default_groups: Vec<String> = body
        .get("default_groups")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| vec!["users".into()]);
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();

    if !oauth_rule_types().contains(&rule_type) {
        return bad(format!("type must be one of: {}", oauth_rule_types().join(", ")))
            .into_response();
    }
    if value.is_empty() {
        return bad("value required").into_response();
    }
    let dup = state
        .auth
        .store
        .list_rules()
        .iter()
        .any(|r| r.rule_type == rule_type && r.value == value);
    if dup {
        return ApiError(
            StatusCode::CONFLICT,
            format!("Rule for {rule_type}={value} already exists"),
        )
        .into_response();
    }
    let rule = OAuthAccessRule {
        rule_id: rule_id_new(),
        rule_type: rule_type.into(),
        value,
        default_groups,
        description,
        created_at: chrono::Utc::now(),
        created_by: "admin".into(),
    };
    match state.auth.store.insert_rule(&rule) {
        Ok(()) => (StatusCode::CREATED, Json(json!(rule))).into_response(),
        Err(e) => internal(e.to_string()).into_response(),
    }
}

pub async fn oauth_rule_update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(rule_id): Path<String>,
    Json(body): Json<Value>,
) -> AuthResult {
    current_user(state.as_ref(), &headers)?;
    let rule_type = body.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let value = body.get("value").and_then(|v| v.as_str()).unwrap_or("").trim().to_lowercase();
    let default_groups: Vec<String> = body
        .get("default_groups")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| vec!["users".into()]);
    let description = body.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !oauth_rule_types().contains(&rule_type) || value.is_empty() {
        return Err(bad("invalid rule"));
    }
    let existing = state
        .auth
        .store
        .list_rules()
        .into_iter()
        .find(|r| r.rule_id == rule_id)
        .ok_or_else(|| not_found("Rule not found"))?;
    let updated = OAuthAccessRule {
        rule_id: existing.rule_id.clone(),
        rule_type: rule_type.into(),
        value,
        default_groups,
        description,
        created_at: existing.created_at,
        created_by: existing.created_by,
    };
    state
        .auth
        .store
        .update_rule(&rule_id, &updated)
        .map_err(|e| internal(e.to_string()))?;
    Ok(Json(json!({ "updated": true, "rule_id": rule_id })))
}

pub async fn oauth_rule_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(rule_id): Path<String>,
) -> AuthResult {
    current_user(state.as_ref(), &headers)?;
    let deleted = state
        .auth
        .store
        .delete_rule(&rule_id)
        .map_err(|e| internal(e.to_string()))?;
    if !deleted {
        return Err(not_found("Rule not found"));
    }
    Ok(Json(json!({ "deleted": true, "rule_id": rule_id })))
}

pub async fn ya360_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> AuthResult {
    current_user(state.as_ref(), &headers)?;
    let y360 = state.auth.oauth.y360.as_ref();
    Ok(Json(json!({
        "configured": y360.map(|c| c.is_configured()).unwrap_or(false),
        "auth_method": y360.map(|c| c.auth_method()).unwrap_or("none"),
        "org_id": y360.map(|c| c.org_id.clone()).unwrap_or_default(),
        "has_token": y360.map(|c| !c.admin_token.is_empty()).unwrap_or(false),
        "has_app_credentials": y360.map(|c| c.has_app_credentials()).unwrap_or(false),
    })))
}
