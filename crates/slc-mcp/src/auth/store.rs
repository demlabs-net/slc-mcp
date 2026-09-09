//! Персистентное хранилище авторизации в vault: `.slc/auth/`.
//! users.json + oauth_rules.json (JSON), audit.jsonl (append).
//! Один процесс владеет vault'ом — файлы читаются при старте, пишутся
//! атомарно (tmp + rename) при каждом изменении.

use super::models::{AuditEntry, OAuthAccessRule, User};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct AuthStore {
    dir: PathBuf,
    users: Mutex<Vec<User>>,
    rules: Mutex<Vec<OAuthAccessRule>>,
}

impl AuthStore {
    pub fn new(vault_path: &str) -> Self {
        Self {
            dir: Path::new(vault_path).join(".slc").join("auth"),
            users: Mutex::new(Vec::new()),
            rules: Mutex::new(Vec::new()),
        }
    }

    /// Загрузить состояние с диска (вызывается при старте сервера).
    pub fn load(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir).context("auth dir create")?;
        let users_file = self.dir.join("users.json");
        if users_file.is_file() {
            let data: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&users_file).context("users.json read")?,
            )
            .context("users.json parse")?;
            let list = data
                .get("users")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            let users: Vec<User> = serde_json::from_value(list).context("users list parse")?;
            *self.users.lock().unwrap() = users;
        }
        let rules_file = self.dir.join("oauth_rules.json");
        if rules_file.is_file() {
            let data: serde_json::Value = serde_json::from_str(
                &std::fs::read_to_string(&rules_file).context("oauth_rules.json read")?,
            )
            .context("oauth_rules.json parse")?;
            let list = data
                .get("rules")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([]));
            let rules: Vec<OAuthAccessRule> =
                serde_json::from_value(list).context("rules list parse")?;
            *self.rules.lock().unwrap() = rules;
        }
        Ok(())
    }

    // ── users ───────────────────────────────────────────────────────────

    fn save_users(&self) -> Result<()> {
        let users = self.users.lock().unwrap();
        let data = serde_json::json!({ "users": &*users });
        atomic_write(
            &self.dir.join("users.json"),
            &serde_json::to_vec_pretty(&data)?,
        )
    }

    pub fn list_users(&self) -> Vec<User> {
        self.users.lock().unwrap().clone()
    }

    pub fn get_user(&self, user_id: &str) -> Result<Option<User>> {
        Ok(self
            .users
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.user_id == user_id)
            .cloned())
    }

    pub fn find_by_username(&self, username: &str) -> Option<User> {
        self.users
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.username == username)
            .cloned()
    }

    pub fn find_by_email(&self, email: &str) -> Option<User> {
        self.users
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.email == email)
            .cloned()
    }

    pub fn find_by_oauth(&self, provider: &str, oauth_id: &str) -> Option<User> {
        self.users
            .lock()
            .unwrap()
            .iter()
            .find(|u| {
                u.oauth_provider.as_deref() == Some(provider)
                    && u.oauth_id.as_deref() == Some(oauth_id)
            })
            .cloned()
    }

    pub fn insert_user(&self, user: &User) -> Result<()> {
        self.users.lock().unwrap().push(user.clone());
        self.save_users()
    }

    pub fn update_user(&self, user: &User) -> Result<()> {
        let mut users = self.users.lock().unwrap();
        if let Some(existing) = users.iter_mut().find(|u| u.user_id == user.user_id) {
            *existing = user.clone();
        } else {
            users.push(user.clone());
        }
        drop(users);
        self.save_users()
    }

    // ── oauth rules ─────────────────────────────────────────────────────

    fn save_rules(&self) -> Result<()> {
        let rules = self.rules.lock().unwrap();
        let data = serde_json::json!({ "rules": &*rules });
        atomic_write(
            &self.dir.join("oauth_rules.json"),
            &serde_json::to_vec_pretty(&data)?,
        )
    }

    pub fn list_rules(&self) -> Vec<OAuthAccessRule> {
        self.rules.lock().unwrap().clone()
    }

    pub fn insert_rule(&self, rule: &OAuthAccessRule) -> Result<()> {
        self.rules.lock().unwrap().push(rule.clone());
        self.save_rules()
    }

    pub fn update_rule(&self, rule_id: &str, rule: &OAuthAccessRule) -> Result<bool> {
        let mut rules = self.rules.lock().unwrap();
        let Some(existing) = rules.iter_mut().find(|r| r.rule_id == rule_id) else {
            return Ok(false);
        };
        *existing = rule.clone();
        drop(rules);
        self.save_rules()?;
        Ok(true)
    }

    pub fn delete_rule(&self, rule_id: &str) -> Result<bool> {
        let mut rules = self.rules.lock().unwrap();
        let before = rules.len();
        rules.retain(|r| r.rule_id != rule_id);
        let removed = rules.len() != before;
        drop(rules);
        if removed {
            self.save_rules()?;
        }
        Ok(removed)
    }

    // ── audit ───────────────────────────────────────────────────────────

    pub fn append_audit(&self, entry: &AuditEntry) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let line = serde_json::to_string(entry)?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("audit.jsonl"))?;
        writeln!(f, "{line}")?;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
