//! JWT manager — 1:1 with legacy `src/auth/jwt_manager.py`:
//! HS256, access 30 minutes, refresh 7 days, claims `type: access|refresh`.

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

pub const DEFAULT_SECRET: &str = "change-me-in-production";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    pub exp: usize,
    pub iat: usize,
    #[serde(rename = "type")]
    pub token_type: String,
}

pub struct JwtManager {
    pub secret: String,
    access_minutes: i64,
    refresh_days: i64,
}

impl JwtManager {
    pub fn from_env() -> Self {
        let secret = std::env::var("JWT_SECRET_KEY").unwrap_or_else(|_| DEFAULT_SECRET.into());
        let access_minutes = std::env::var("JWT_ACCESS_TOKEN_EXPIRE_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let refresh_days = std::env::var("JWT_REFRESH_TOKEN_EXPIRE_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7);
        Self {
            secret,
            access_minutes,
            refresh_days,
        }
    }

    fn encode(&self, claims: Claims) -> Result<String, String> {
        encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(self.secret.as_bytes()),
        )
        .map_err(|e| e.to_string())
    }

    fn decode(&self, token: &str) -> Result<Claims, String> {
        let mut validation = Validation::new(Algorithm::HS256);
        // exp is checked manually (jsonwebtoken does not expose the expiry time).
        validation.validate_exp = false;
        let data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(self.secret.as_bytes()),
            &validation,
        )
        .map_err(|e| e.to_string())?;
        if data.claims.exp < now_ts() {
            return Err("token expired".into());
        }
        Ok(data.claims)
    }

    pub fn create_access_token(
        &self,
        user_id: &str,
        username: &str,
        groups: &[String],
    ) -> Result<String, String> {
        let now = now_ts();
        self.encode(Claims {
            sub: user_id.into(),
            username: Some(username.into()),
            groups: groups.to_vec(),
            iat: now,
            exp: now + (self.access_minutes * 60) as usize,
            token_type: "access".into(),
        })
    }

    pub fn create_refresh_token(&self, user_id: &str) -> Result<String, String> {
        let now = now_ts();
        self.encode(Claims {
            sub: user_id.into(),
            username: None,
            groups: vec![],
            iat: now,
            exp: now + (self.refresh_days * 86400) as usize,
            token_type: "refresh".into(),
        })
    }

    /// Validate an access token (legacy `verify_token` — access type only).
    pub fn verify_access(&self, token: &str) -> Result<Claims, String> {
        let claims = self.decode(token)?;
        if claims.token_type != "access" {
            return Err("not an access token".into());
        }
        Ok(claims)
    }

    /// Validate a refresh token; returns the user_id (legacy
    /// `verify_refresh_token`).
    pub fn verify_refresh(&self, token: &str) -> Result<String, String> {
        let claims = self.decode(token)?;
        if claims.token_type != "refresh" {
            return Err("not a refresh token".into());
        }
        Ok(claims.sub)
    }
}

fn now_ts() -> usize {
    chrono::Utc::now().timestamp() as usize
}
