//! Obsidian vault backend — the DEFAULT storage for SLC.
//!
//! A vault is a directory of human-readable markdown notes organized in
//! folders (Obsidian is a folder-based vault). Layout:
//!
//! ```text
//! vault/
//!   core/ modules/ tasks/ projects/ docs/ code/ custom/ system/
//!     <document_id>.md            ← KB documents, human-readable names
//!   history/YYYY/MM/<document_id>.md   ← episodic (diary layout by date)
//!   seats/<seat_id>.json          ← seats (operational data)
//!   .slc/
//!     index.json                  ← write-only snapshot of the live index for
//!                                     inspection; never read back — the live
//!                                     index is rebuilt from file frontmatter
//!                                     on open() and every refresh()
//!     embeddings.json             ← embedding chunks
//!     timers.json                 ← persisted timers
//!     records.json                ← free-form JSON records
//! ```
//!
//! Each note: YAML frontmatter (all fields except `content`) + markdown body
//! (the document content). Files are the source of truth — the index is
//! rebuilt on open so hand-edits in Obsidian are picked up. Optional git
//! auto-commit keeps the vault versioned (default off).
//!
//! KB and episodic live in separate folder trees (history never gets
//! embedded/searched), mirroring the [`StorageBackend`] split.

use super::{DocFilter, DocSort, KbMetadataProjection, MetaPatch, SortDir, StorageBackend};
use crate::error::{SlcError, SlcResult};
use crate::model::{
    Document, DocumentCategory, EmbeddingRecord, EmbeddingScope, PersistedTimer, Seat, SeatStatus,
    content_hash,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const GIT_INDEX_LOCK_STALE_AFTER: Duration = Duration::from_secs(5 * 60);
const GIT_COMMIT_DEBOUNCE: Duration = Duration::from_secs(5);
/// Poll interval while a shutdown flush waits for the coalesced snapshot
/// worker. Only the shutdown path pays for it.
const FLUSH_POLL_INTERVAL: Duration = Duration::from_millis(20);
const GIT_MAINTENANCE_CHECK_INTERVAL: u64 = 16;
const GIT_MAINTENANCE_LOOSE_OBJECT_LIMIT: u64 = 1024;
const GIT_MAINTENANCE_LOOSE_KIB_LIMIT: u64 = 128 * 1024;
/// `records.json` is the runtime record store (paginated responses, task-event
/// idempotency): it is rewritten on almost every write, so versioning it made
/// each commit add a multi-megabyte blob and filled the vault's disk. It stays
/// on disk and is loaded at startup, it is just not versioned.
const DERIVED_SIDECAR_IGNORE_BLOCK: &str = "# SLC derived caches (rebuilt locally; do not version)\n/.slc/index.json\n/.slc/embeddings.json\n/.slc/records.json\n";
static ATOMIC_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static GIT_COMMITS_SINCE_START: AtomicU64 = AtomicU64::new(0);
/// Emit the "no remote" explanation at most once per process. Without this the
/// skip would be reported on every commit — the same noise the skip removes.
static GIT_PUSH_SKIPPED_NO_REMOTE_LOGGED: AtomicBool = AtomicBool::new(false);
// Git's automatic maintenance can intentionally detach a `git` child after
// commits. It is then reparented to the SLC container's PID 1, which has no
// generic child reaper, leaving an exited maintenance process as a zombie.
// Vault snapshots are already scheduled by SLC; disable overlapping Git work.
const GIT_NO_BACKGROUND_MAINTENANCE: &[&str] = &[
    "-c",
    "maintenance.auto=false",
    "-c",
    "gc.auto=0",
    "-c",
    "gc.autoDetach=false",
];

/// Replace a vault file without truncating the inode a concurrent `git add`
/// may already have mmaped. Truncating that inode can terminate Git with
/// SIGBUS, leave index.lock behind, and stall all later snapshots. Temporary
/// files live under `.git` when available so Git never stages an in-flight
/// write; rename is atomic because both paths remain on the vault filesystem.
fn atomic_replace(temp_dir: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(temp_dir)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidInput, "invalid vault file name"))?;
    for _ in 0..32 {
        let sequence = ATOMIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = temp_dir.join(format!("{file_name}.{}.{}", std::process::id(), sequence));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(bytes).and_then(|_| file.flush()) {
            drop(file);
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        drop(file);
        if let Err(error) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        return Ok(());
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "could not allocate a unique vault temporary file",
    ))
}
fn quarantine_git_index_lock(
    root: &Path,
    now: SystemTime,
    minimum_age: Duration,
) -> std::io::Result<Option<PathBuf>> {
    let lock = root.join(".git/index.lock");
    let metadata = match std::fs::symlink_metadata(&lock) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Ok(None);
    }
    let modified = metadata.modified()?;
    let age = now.duration_since(modified).unwrap_or_default();
    if age < minimum_age {
        return Ok(None);
    }

    let quarantine_dir = root.join(".git/slc-stale-index-locks");
    std::fs::create_dir_all(&quarantine_dir)?;
    let timestamp = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let quarantined = quarantine_dir.join(format!("index.lock.{timestamp}"));
    std::fs::rename(&lock, &quarantined)?;
    Ok(Some(quarantined))
}

/// Preserve an abandoned Git index lock once it is old enough that it cannot
/// belong to this store's bounded Git worker. The worker coalescer guarantees
/// one in-process Git sequence at a time; the age guard avoids disrupting a
/// short-lived external Git command operating on the same vault.
fn quarantine_stale_git_index_lock(
    root: &Path,
    now: SystemTime,
) -> std::io::Result<Option<PathBuf>> {
    quarantine_git_index_lock(root, now, GIT_INDEX_LOCK_STALE_AFTER)
}

fn parse_git_count_objects(output: &str) -> Option<(u64, u64)> {
    let mut count = None;
    let mut size_kib = None;
    for line in output.lines() {
        let Some((key, raw_value)) = line.split_once(':') else {
            continue;
        };
        let value = raw_value.trim().parse::<u64>().ok();
        match key.trim() {
            "count" => count = value,
            "size" => size_kib = value,
            _ => {}
        }
    }
    count.zip(size_kib)
}

/// Keep rebuildable sidecars out of vault history. `index.json` is rebuilt
/// from Markdown on every open, embeddings are a local search cache, and
/// `records.json` is the runtime record store that is rewritten on nearly every
/// write; all three stay on disk, but none of them belongs in Git history.
fn ensure_derived_sidecars_ignored(root: &Path) -> std::io::Result<()> {
    let path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let required = [
        "/.slc/index.json",
        "/.slc/embeddings.json",
        "/.slc/records.json",
    ];
    if required
        .iter()
        .all(|entry| existing.lines().any(|line| line.trim() == *entry))
    {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(DERIVED_SIDECAR_IGNORE_BLOCK);
    atomic_replace(&root.join(".git/slc-write-tmp"), &path, updated.as_bytes())
}

/// What the post-commit push actually did. Reported so operators and tests can
/// see the outcome directly instead of parsing logs; the coalesced worker
/// ignores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultPush {
    /// The vault has no remote: no `git push` process was started at all.
    SkippedNoRemote,
    /// The commit itself failed, so there was nothing to push.
    NoCommit,
    /// `git push` ran and succeeded.
    Pushed,
    /// `git push` ran and failed (stderr kept for the debug line).
    Failed(String),
}

/// How a shutdown-time flush of the coalesced snapshot ended. Reported so the
/// shutdown path can log a truthful outcome instead of guessing, and so tests
/// can tell "nothing was pending" from "the worker finished it for us".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultFlush {
    /// Auto-commit is off, or no write was pending: nothing to flush.
    Idle,
    /// The coalesced worker was still busy when the bound elapsed. The flush
    /// deliberately starts no second snapshot — the in-flight worker owns the
    /// repository. Whatever it snapshots on its own is complete; the wait was
    /// simply shorter than its debounce.
    WorkerBusy,
    /// The worker was still running, but finished its own pass inside the
    /// bound; the pending writes are in the repository.
    DrainedByWorker,
    /// The flush owned the dirty flag and ran the final snapshot itself.
    Committed(VaultPush),
    /// The snapshot task itself failed to run (the Git process could not be
    /// started at all, not a Git failure — that is reported by [`VaultPush`]).
    Failed(String),
}

/// Whether the vault has any Git remote configured.
///
/// Asking Git (instead of parsing `.git/config`) stays correct for every config
/// shape: `origin`, other remote names, `url.<base>.insteadOf` rewrites, and a
/// worktree whose config lives elsewhere. It is a purely local read — no
/// network, no credentials, no terminal prompt.
fn vault_remote_configured(root: &Path) -> bool {
    match std::process::Command::new("git")
        .args(GIT_NO_BACKGROUND_MAINTENANCE)
        .arg("remote")
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
    {
        Ok(output) if output.status.success() => {
            !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        }
        // Not a repository (or Git refused): there is nothing to push to. The
        // commit path reports the failure that actually matters.
        Ok(_) => false,
        Err(_) => false,
    }
}

/// Run one coalesced Git snapshot for the vault. This is intentionally invoked
/// only by the coalesced worker below: the storage write path must never queue
/// one Git process per document or seat update.
fn run_vault_git_commit(root: &Path, author: &str) -> VaultPush {
    let author_name = author
        .split('<')
        .next()
        .unwrap_or("slc-mcp")
        .trim()
        .to_string();
    let author_email = author
        .split_once('<')
        .and_then(|(_, email)| email.strip_suffix('>'))
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .unwrap_or("slc-mcp@local")
        .to_string();
    match quarantine_stale_git_index_lock(root, SystemTime::now()) {
        Ok(Some(path)) => tracing::warn!(
            path = %path.display(),
            "vault git: quarantined abandoned index lock"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!("vault git lock recovery error: {error}"),
    }
    let run = |args: Vec<&str>| {
        // The coalesced worker is deliberately detached from request handling,
        // so direct Git waiting cannot stall SLC. Avoid an external `timeout`
        // wrapper here: it can orphan/reparent `git` under a long-lived PID 1
        // and leak zombies after a cancelled snapshot.
        let mut cmd = std::process::Command::new("git");
        cmd.args(GIT_NO_BACKGROUND_MAINTENANCE)
            .args(&args)
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", &author_name)
            .env("GIT_AUTHOR_EMAIL", &author_email)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
    };
    let failed = |out: &std::process::Output| {
        tracing::warn!("vault git: {}", String::from_utf8_lossy(&out.stderr).trim());
    };
    // Existing vaults may already track the derived caches. Removing them
    // from the index is non-destructive (`--cached`) and, together with the
    // managed ignore rules, prevents `git add -A` from reintroducing them.
    match run(vec![
        "rm",
        "--cached",
        "--ignore-unmatch",
        "--",
        ".slc/index.json",
        ".slc/embeddings.json",
        ".slc/records.json",
    ]) {
        Ok(out) if out.status.success() => {}
        Ok(out) => failed(&out),
        Err(error) => tracing::warn!("vault git untrack derived sidecars error: {error}"),
    }
    match run(vec!["add", "-A"]) {
        Ok(out) if out.status.success() => {
            // A read-only lifecycle call may still persist operational
            // metadata. Avoid manufacturing empty commits when the vault
            // representation did not actually change.
            match run(vec!["diff", "--cached", "--quiet"]) {
                Ok(diff) if diff.status.success() => return VaultPush::NoCommit,
                Ok(diff) if diff.status.code() == Some(1) => {}
                Ok(diff) => {
                    failed(&diff);
                    return VaultPush::NoCommit;
                }
                Err(error) => {
                    tracing::warn!("vault git diff error: {error}");
                    return VaultPush::NoCommit;
                }
            }
            let commit = run(vec!["commit", "-m", "slc: vault update"]);
            match &commit {
                Ok(commit) if commit.status.success() => {
                    let outcome = push_vault(root, &run);
                    let committed = GIT_COMMITS_SINCE_START.fetch_add(1, Ordering::Relaxed) + 1;
                    if committed == 1 || committed % GIT_MAINTENANCE_CHECK_INTERVAL == 0 {
                        match run(vec!["count-objects", "-v"]) {
                            Ok(count_objects) if count_objects.status.success() => {
                                let summary = String::from_utf8_lossy(&count_objects.stdout);
                                if let Some((loose_count, loose_kib)) =
                                    parse_git_count_objects(&summary)
                                {
                                    if loose_count >= GIT_MAINTENANCE_LOOSE_OBJECT_LIMIT
                                        || loose_kib >= GIT_MAINTENANCE_LOOSE_KIB_LIMIT
                                    {
                                        tracing::info!(
                                            loose_count,
                                            loose_kib,
                                            "vault git: compacting loose objects"
                                        );
                                        match run(vec!["gc", "--prune=2.weeks.ago"]) {
                                            Ok(gc) if gc.status.success() => {}
                                            Ok(gc) => failed(&gc),
                                            Err(error) => {
                                                tracing::warn!("vault git gc error: {error}")
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(count_objects) => failed(&count_objects),
                            Err(error) => {
                                tracing::warn!("vault git count-objects error: {error}")
                            }
                        }
                    }
                    outcome
                }
                Ok(commit) => {
                    failed(commit);
                    VaultPush::NoCommit
                }
                Err(error) => {
                    tracing::warn!("vault git commit error: {error}");
                    VaultPush::NoCommit
                }
            }
        }
        Ok(out) => {
            failed(&out);
            VaultPush::NoCommit
        }
        Err(error) => {
            tracing::warn!("vault git add error: {error}");
            VaultPush::NoCommit
        }
    }
}

/// Push a committed snapshot, but only when the vault actually has a remote.
///
/// A vault with no remote (the normal state of a local swarm vault) has no
/// upstream: running `git push` there cannot succeed, so every auto-commit paid
/// for a failing process plus a journal line. That constant stream buried the
/// diagnostics that mattered, so the no-remote case is now not attempted at all
/// and reported at most once per process, at debug level.
fn push_vault(
    root: &Path,
    run: &impl Fn(Vec<&str>) -> std::io::Result<std::process::Output>,
) -> VaultPush {
    if !vault_remote_configured(root) {
        if !GIT_PUSH_SKIPPED_NO_REMOTE_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::debug!(
                "vault git: no remote configured — vault push is skipped (vault is committed locally)"
            );
        }
        return VaultPush::SkippedNoRemote;
    }
    match run(vec!["push", "-q"]) {
        Ok(push) if push.status.success() => VaultPush::Pushed,
        Ok(push) => {
            let stderr = String::from_utf8_lossy(&push.stderr).trim().to_string();
            tracing::debug!("vault push: {stderr}");
            VaultPush::Failed(stderr)
        }
        Err(error) => {
            tracing::debug!("vault push error: {error}");
            VaultPush::Failed(error.to_string())
        }
    }
}

/// Return whether the sole coalesced worker should immediately run another
/// pass. The post-release recheck closes the race where a write arrives after
/// the first idle observation but before a new worker can be scheduled.
fn finish_git_commit_worker(dirty: &AtomicBool, worker_active: &AtomicBool) -> bool {
    if dirty.swap(false, Ordering::AcqRel) {
        return true;
    }
    worker_active.store(false, Ordering::Release);
    dirty.load(Ordering::Acquire)
        && worker_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
}

/// Human-readable file name from a unique document id.
/// Keeps letters/digits/`-`/`_`/`.`/space; path-hostile chars → `-`.
/// Validate a vault folder path from an untrusted document. Nested
/// relative paths are fine (`projects/vassista`, `history/2026/08` —
/// `default_folder` builds those from categories and ids); anything that
/// could escape the vault root is rejected: absolute paths, `..`
/// components, backslashes, colons and control characters.
pub fn sanitize_folder(folder: &str) -> SlcResult<String> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err(SlcError::Storage("folder must not be empty".into()));
    }
    if folder.starts_with('/') {
        return Err(SlcError::Storage(format!(
            "folder must be relative to the vault: {folder}"
        )));
    }
    for part in folder.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(SlcError::Storage(format!(
                "folder contains an invalid component: {folder}"
            )));
        }
        if part
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':'))
        {
            return Err(SlcError::Storage(format!(
                "folder contains invalid characters: {folder}"
            )));
        }
    }
    Ok(folder.to_string())
}

pub fn safe_file_name(document_id: &str) -> String {
    let mut out = String::with_capacity(document_id.len());
    for c in document_id.chars() {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("doc");
    }
    out
}

// ─────────────────────────────── index ───────────────────────────────

/// Filter fields for one document (mirrors `DocFilter`; source of truth for
/// fast listing/counting without reading note bodies).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub folder: String,
    pub category: String,
    pub seat_id: Option<String>,
    pub doc_level: Option<String>,
    pub doc_type: Option<String>,
    #[serde(default)]
    pub extra_strings: BTreeMap<String, String>,
    /// Full free-form metadata projection. The sidecar is rebuildable and this
    /// avoids reopening every Markdown body for metadata-only workflow scans.
    #[serde(default)]
    pub metadata_extra: Map<String, Value>,
    pub archived: Option<bool>,
    pub consolidated: Option<bool>,
    pub compression_batch_id: Option<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub version: i64,
    pub deleted_at: Option<String>,
    /// True when `id` came from SLC frontmatter, false when derived from the
    /// file name. Not persisted; used to resolve id collisions at scan time.
    #[serde(skip)]
    pub explicit_id: bool,
    /// Actual note file stem (without `.md`). Set for Obsidian notes whose
    /// `id` was derived from the file name, so reads/writes hit the real file
    /// even when the name contains characters `safe_file_name` would mangle.
    #[serde(default)]
    pub file_name: Option<String>,
}

impl IndexEntry {
    fn from_doc(doc: &Document) -> Self {
        IndexEntry {
            id: doc.document_id.clone(),
            folder: doc.folder.clone().unwrap_or_else(|| doc.default_folder()),
            category: doc.category.as_str().to_string(),
            seat_id: doc.seat_id.clone(),
            doc_level: doc.metadata.doc_level.map(|l| l.as_str().to_string()),
            doc_type: doc.metadata.doc_type.clone(),
            extra_strings: doc
                .metadata
                .extra
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect(),
            metadata_extra: doc.metadata.extra.clone(),
            archived: doc.metadata.archived,
            consolidated: doc.metadata.consolidated,
            compression_batch_id: doc.metadata.compression_batch_id.clone(),
            tags: doc.tags.clone(),
            created_at: doc.created_at.to_rfc3339(),
            updated_at: doc.updated_at.to_rfc3339(),
            version: doc.version,
            deleted_at: doc.deleted_at.map(|d| d.to_rfc3339()),
            explicit_id: true,
            file_name: None,
        }
    }

    fn matches(&self, f: &DocFilter) -> bool {
        if let Some(cat) = f.category {
            if self.category != cat.as_str() {
                return false;
            }
        }
        if let Some(seat) = &f.seat_id {
            if self.seat_id.as_deref() != Some(seat.as_str()) {
                return false;
            }
        }
        if let Some(vis) = &f.visible_to {
            if let Some(owner) = &self.seat_id {
                if owner != vis {
                    return false;
                }
            }
        }
        if let Some(level) = f.doc_level {
            if self.doc_level.as_deref() != Some(level.as_str()) {
                return false;
            }
        }
        if let Some(ids) = &f.document_ids {
            if !ids.iter().any(|id| id == &self.id) {
                return false;
            }
        }
        if let Some(dt) = &f.doc_type {
            if self.doc_type.as_deref() != Some(dt.as_str()) {
                return false;
            }
        }
        if f.extra_strings
            .iter()
            .any(|(key, expected)| self.extra_strings.get(key) != Some(expected))
        {
            return false;
        }
        if f.extra_strings_not_in.iter().any(|(key, excluded)| {
            self.extra_strings
                .get(key)
                .is_some_and(|value| excluded.contains(value))
        }) {
            return false;
        }
        if let Some(archived) = f.archived {
            if self.archived != Some(archived) {
                return false;
            }
        }
        if f.not_consolidated && self.consolidated == Some(true) {
            return false;
        }
        match f.has_compression_batch {
            Some(true) => {
                if self.compression_batch_id.is_none() {
                    return false;
                }
            }
            Some(false) => {
                if self.compression_batch_id.is_some() {
                    return false;
                }
            }
            None => {}
        }
        if !f.tags_any.is_empty() && !f.tags_any.iter().any(|t| self.tags.contains(t)) {
            return false;
        }
        if !f.tags_all.is_empty() && !f.tags_all.iter().all(|t| self.tags.contains(t)) {
            return false;
        }
        if f.deleted {
            if self.deleted_at.is_none() {
                return false;
            }
        } else if self.deleted_at.is_some() {
            return false;
        }
        let updated = self.updated_at.parse::<DateTime<Utc>>().ok();
        if let (Some(since), Some(u)) = (f.since, updated) {
            if u < since {
                return false;
            }
        }
        if let (Some(until), Some(u)) = (f.until, updated) {
            if u > until {
                return false;
            }
        }
        true
    }
}

// ─────────────────────────────── store ───────────────────────────────

/// Obsidian vault backend — DEFAULT. See module docs for layout.
#[derive(Clone)]
pub struct ObsidianVaultStore {
    root: PathBuf,
    index: std::sync::Arc<Mutex<HashMap<String, IndexEntry>>>,
    embeddings: std::sync::Arc<Mutex<HashMap<String, Vec<EmbeddingRecord>>>>,
    seats: std::sync::Arc<Mutex<HashMap<String, Seat>>>,
    timers: std::sync::Arc<Mutex<HashMap<String, PersistedTimer>>>,
    records: std::sync::Arc<Mutex<HashMap<(String, String), Value>>>,
    auto_git_commit: bool,
    git_author: String,
    /// Writes mark the vault dirty. Exactly one coalesced worker drains that
    /// flag, so high-frequency seat touches cannot build an unbounded queue of
    /// git add/commit processes.
    git_commit_dirty: Arc<AtomicBool>,
    git_commit_worker_active: Arc<AtomicBool>,
}

impl ObsidianVaultStore {
    /// Open a vault directory (created if missing). `auto_git_commit` runs
    /// `git add -A && git commit` after every write (vault must be a repo).
    pub fn open(root: impl AsRef<Path>, auto_git_commit: bool) -> SlcResult<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join(".slc"))?;
        std::fs::create_dir_all(root.join("seats"))?;
        if auto_git_commit && root.join(".git").is_dir() {
            ensure_derived_sidecars_ignored(&root)?;
        }
        let store = ObsidianVaultStore {
            root,
            index: std::sync::Arc::new(Mutex::new(HashMap::new())),
            embeddings: std::sync::Arc::new(Mutex::new(HashMap::new())),
            seats: std::sync::Arc::new(Mutex::new(HashMap::new())),
            timers: std::sync::Arc::new(Mutex::new(HashMap::new())),
            records: std::sync::Arc::new(Mutex::new(HashMap::new())),
            auto_git_commit,
            git_author: std::env::var("OBSIDIAN_GIT_AUTHOR")
                .unwrap_or_else(|_| "slc-mcp <slc-mcp@local>".into()),
            git_commit_dirty: Arc::new(AtomicBool::new(false)),
            git_commit_worker_active: Arc::new(AtomicBool::new(false)),
        };
        store.rebuild_index()?;
        store.load_sidecars()?;
        Ok(store)
    }

    fn slc_dir(&self) -> PathBuf {
        self.root.join(".slc")
    }

    fn atomic_write(&self, path: &Path, bytes: &[u8]) -> SlcResult<()> {
        let git_temp = self.root.join(".git/slc-write-tmp");
        let temp_dir = if self.root.join(".git").is_dir() {
            git_temp
        } else {
            self.slc_dir().join("write-tmp")
        };
        atomic_replace(&temp_dir, path, bytes)?;
        Ok(())
    }

    /// Scan the vault and rebuild the document index from file frontmatter
    /// (picks up hand-edits made in Obsidian).
    pub fn rebuild_index(&self) -> SlcResult<()> {
        let mut index = HashMap::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)?;
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    let name = e.file_name();
                    let name = name.to_string_lossy().to_string();
                    if name == ".git" || name == ".slc" || name == "seats" {
                        continue;
                    }
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "md") {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let (meta, _) = frontmatter_parse(&text);
                    let meta = meta.unwrap_or_default();
                    let explicit_id = meta
                        .get("id")
                        .and_then(|v| v.as_str())
                        .filter(|id| !id.is_empty())
                        .map(str::to_string);
                    let rel = path
                        .strip_prefix(&self.root)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default();
                    // Obsidian notes without SLC frontmatter (no `id`) get a
                    // document_id derived from the file name, so the whole
                    // vault ends up in the index.
                    let derived_name = explicit_id.is_none().then(|| {
                        path.file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default()
                    });
                    let id = explicit_id
                        .clone()
                        .unwrap_or_else(|| derived_name.clone().unwrap_or_default());
                    if id.is_empty() {
                        continue;
                    }
                    let entry = IndexEntry {
                        id: id.clone(),
                        folder: Path::new(&rel)
                            .parent()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        category: meta
                            .get("category")
                            .and_then(|v| v.as_str())
                            .unwrap_or("custom")
                            .to_string(),
                        seat_id: meta
                            .get("seat_id")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        doc_level: meta
                            .get("metadata")
                            .and_then(|m| m.get("doc_level"))
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        doc_type: meta
                            .get("metadata")
                            .and_then(|m| m.get("doc_type"))
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        extra_strings: meta
                            .get("metadata")
                            .and_then(|v| v.as_object())
                            .map(|extra| {
                                extra
                                    .iter()
                                    .filter_map(|(key, value)| {
                                        value.as_str().map(|value| (key.clone(), value.to_string()))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        metadata_extra: meta
                            .get("metadata")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default(),
                        archived: meta
                            .get("metadata")
                            .and_then(|m| m.get("archived"))
                            .and_then(|v| v.as_bool()),
                        consolidated: meta
                            .get("metadata")
                            .and_then(|m| m.get("consolidated"))
                            .and_then(|v| v.as_bool()),
                        compression_batch_id: meta
                            .get("metadata")
                            .and_then(|m| m.get("compression_batch_id"))
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        tags: meta
                            .get("tags")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        created_at: meta
                            .get("created_at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        updated_at: meta
                            .get("updated_at")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        version: meta.get("version").and_then(|v| v.as_i64()).unwrap_or(1),
                        deleted_at: meta
                            .get("deleted_at")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        explicit_id: explicit_id.is_some(),
                        file_name: derived_name,
                    };
                    match index.entry(id.clone()) {
                        std::collections::hash_map::Entry::Vacant(slot) => {
                            slot.insert(entry);
                        }
                        std::collections::hash_map::Entry::Occupied(mut slot) => {
                            if explicit_id.is_some() && !slot.get().explicit_id {
                                // explicit id (SLC document) overrides the derived one
                                slot.insert(entry);
                            } else if explicit_id.is_some() && slot.get().explicit_id {
                                return Err(SlcError::Storage(format!(
                                    "duplicate document id {id} in vault folders {:?} and {:?}",
                                    slot.get().folder,
                                    entry.folder
                                )));
                            }
                            // derived id already taken: keep the first file
                        }
                    }
                }
            }
        }
        *self.index.lock().unwrap() = index;
        self.persist_index()
    }

    fn load_sidecars(&self) -> SlcResult<()> {
        let emb_path = self.slc_dir().join("embeddings.json");
        if let Ok(text) = std::fs::read_to_string(&emb_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, Vec<EmbeddingRecord>>>(&text) {
                *self.embeddings.lock().unwrap() = map;
            }
        }
        let timers_path = self.slc_dir().join("timers.json");
        if let Ok(text) = std::fs::read_to_string(&timers_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, PersistedTimer>>(&text) {
                *self.timers.lock().unwrap() = map;
            }
        }
        let records_path = self.slc_dir().join("records.json");
        if let Ok(text) = std::fs::read_to_string(&records_path) {
            if let Ok(map) = serde_json::from_str::<HashMap<String, Value>>(&text) {
                let map: HashMap<(String, String), Value> = map
                    .into_iter()
                    .filter_map(|(k, v)| {
                        let (coll, key) = k.split_once('\u{0}')?;
                        Some(((coll.to_string(), key.to_string()), v))
                    })
                    .collect();
                *self.records.lock().unwrap() = map;
            }
        }
        // Seats: one JSON file per seat in seats/.
        let mut seats = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(self.root.join("seats")) {
            for e in rd.flatten() {
                if let Ok(text) = std::fs::read_to_string(e.path()) {
                    if let Ok(seat) = serde_json::from_str::<Seat>(&text) {
                        seats.insert(seat.seat_id.clone(), seat);
                    }
                }
            }
        }
        *self.seats.lock().unwrap() = seats;
        Ok(())
    }

    /// Dump the live in-memory index for inspection/debugging. Nothing reads
    /// this file back: `open()`/`refresh()` rebuild the index from markdown
    /// frontmatter, so hand-edits to the files are picked up without touching
    /// the sidecar (and hand-edits to the sidecar alone have no effect).
    fn persist_index(&self) -> SlcResult<()> {
        let map = self.index.lock().unwrap();
        let text = serde_json::to_string_pretty(&*map)?;
        self.atomic_write(&self.slc_dir().join("index.json"), text.as_bytes())?;
        Ok(())
    }

    fn persist_embeddings(&self) -> SlcResult<()> {
        let map = self.embeddings.lock().unwrap();
        let text = serde_json::to_string_pretty(&*map)?;
        self.atomic_write(&self.slc_dir().join("embeddings.json"), text.as_bytes())?;
        Ok(())
    }

    fn persist_timers(&self) -> SlcResult<()> {
        let map = self.timers.lock().unwrap();
        let text = serde_json::to_string_pretty(&*map)?;
        self.atomic_write(&self.slc_dir().join("timers.json"), text.as_bytes())?;
        Ok(())
    }

    fn persist_records(&self) -> SlcResult<()> {
        let map = self.records.lock().unwrap();
        let flat: HashMap<String, Value> = map
            .iter()
            .map(|((c, k), v)| (format!("{c}\u{0}{k}"), v.clone()))
            .collect();
        let text = serde_json::to_string_pretty(&flat)?;
        self.atomic_write(&self.slc_dir().join("records.json"), text.as_bytes())?;
        Ok(())
    }

    fn persist_seat(&self, seat: &Seat) -> SlcResult<()> {
        let path = self
            .root
            .join("seats")
            .join(format!("{}.json", safe_file_name(&seat.seat_id)));
        let text = serde_json::to_string_pretty(seat)?;
        self.atomic_write(&path, text.as_bytes())?;
        Ok(())
    }

    /// Write a document note: `{folder}/{safe_name}.md` (frontmatter + body).
    fn write_note(&self, doc: &Document) -> SlcResult<()> {
        let folder = doc.folder.clone().unwrap_or_else(|| doc.default_folder());
        let folder = sanitize_folder(&folder)?;
        let dir = self.root.join(&folder);
        std::fs::create_dir_all(&dir)?;
        let previous = self.index.lock().unwrap().get(&doc.document_id).cloned();
        let file_name = previous
            .as_ref()
            .and_then(|e| e.file_name.clone())
            .unwrap_or_else(|| safe_file_name(&doc.document_id));
        let path = dir.join(format!("{file_name}.md"));
        let text = render_doc(doc)?;
        self.atomic_write(&path, text.as_bytes())?;

        // A full replacement may change category or project binding, which
        // changes the canonical folder. Leaving the old note behind creates
        // duplicate IDs and makes the next index rebuild nondeterministic.
        if let Some(previous) = previous {
            if previous.folder != folder {
                let old_path = self
                    .root
                    .join(sanitize_folder(&previous.folder)?)
                    .join(format!("{file_name}.md"));
                if old_path.exists() {
                    std::fs::remove_file(old_path)?;
                }
            }
        }
        self.index
            .lock()
            .unwrap()
            .insert(doc.document_id.clone(), IndexEntry::from_doc(doc));
        self.persist_index()?;
        Ok(())
    }

    /// Read + parse a note into a Document (files are the source of truth).
    fn read_note(&self, entry: &IndexEntry) -> SlcResult<Document> {
        let file_name = entry
            .file_name
            .clone()
            .unwrap_or_else(|| safe_file_name(&entry.id));
        let path = self
            .root
            .join(&sanitize_folder(&entry.folder)?)
            .join(format!("{file_name}.md"));
        let text = std::fs::read_to_string(&path)?;
        let (meta, body) = frontmatter_parse(&text);
        let meta = meta.unwrap_or_default();
        let metadata: crate::model::DocMeta = meta
            .get("metadata")
            .cloned()
            .map(|m| serde_json::from_value(m).unwrap_or_default())
            .unwrap_or_default();
        Ok(Document {
            document_id: meta
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(&entry.id)
                .to_string(),
            category: DocumentCategory::parse(
                meta.get("category")
                    .and_then(|v| v.as_str())
                    .unwrap_or("custom"),
            )
            .unwrap_or(DocumentCategory::Custom),
            folder: Some(entry.folder.clone()),
            content: body,
            content_hash: meta
                .get("content_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            metadata,
            tags: meta
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            auto_load: meta
                .get("auto_load")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            references: meta
                .get("references")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            seat_id: meta
                .get("seat_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: meta
                .get("created_at")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now),
            updated_at: meta
                .get("updated_at")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(Utc::now),
            version: meta
                .get("version")
                .and_then(|v| v.as_i64())
                .unwrap_or(entry.version),
            deleted_at: meta
                .get("deleted_at")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc)),
        })
    }

    /// Collect matching entries (KB or episodic by table), sort them, and
    /// optionally apply a caller-visible bound.
    fn select_with_limit(
        &self,
        table: Table,
        filter: &DocFilter,
        sort: &DocSort,
        limit: Option<usize>,
    ) -> Vec<IndexEntry> {
        let index = self.index.lock().unwrap();
        let mut out: Vec<&IndexEntry> = index
            .values()
            .filter(|e| {
                let is_kb = e.category != "history";
                if table == Table::Kb && !is_kb {
                    return false;
                }
                if table == Table::Episodic && is_kb {
                    return false;
                }
                e.matches(filter)
            })
            .collect();
        out.sort_by(|a, b| {
            let (af, bf) = match sort.field {
                super::SortField::CreatedAt => (&a.created_at, &b.created_at),
                super::SortField::UpdatedAt => (&a.updated_at, &b.updated_at),
            };
            let ord = af.cmp(bf);
            match sort.dir {
                SortDir::Asc => ord,
                SortDir::Desc => ord.reverse(),
            }
        });
        if let Some(limit) = limit {
            out.truncate(limit.min(out.len()));
        }
        out.into_iter().cloned().collect()
    }

    fn select(
        &self,
        table: Table,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> Vec<IndexEntry> {
        self.select_with_limit(table, filter, sort, Some(limit))
    }

    fn select_unbounded(
        &self,
        table: Table,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> Vec<IndexEntry> {
        self.select_with_limit(table, filter, sort, None)
    }

    /// Fire-and-forget, coalesced vault commit. The write path must NEVER
    /// block on Git: a stuck index.lock, a hung push, or a slow `git add -A`
    /// must not stall the MCP request. A burst of writes therefore marks one
    /// shared dirty flag and is drained by at most one bounded worker instead
    /// of queuing one Git process per write.
    fn git_commit(&self) {
        if !self.auto_git_commit {
            return;
        }
        self.git_commit_dirty.store(true, Ordering::Release);
        if self.git_commit_worker_active.swap(true, Ordering::AcqRel) {
            return;
        }
        let root = self.root.clone();
        let author = self.git_author.clone();
        let dirty = self.git_commit_dirty.clone();
        let worker_active = self.git_commit_worker_active.clone();
        let _ = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(GIT_COMMIT_DEBOUNCE).await;
                // The current on-disk state covers all writes seen so far.
                // Any write while Git runs sets this flag again and earns one
                // follow-up pass after the current snapshot finishes.
                dirty.store(false, Ordering::Release);
                let worker_root = root.clone();
                let worker_author = author.clone();
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    // The outcome is reported through logs and the `VaultPush`
                    // enum; the coalesced worker has no caller to return to.
                    let _ = run_vault_git_commit(&worker_root, &worker_author);
                })
                .await
                {
                    tracing::warn!("vault git worker failed: {error}");
                }
                if !finish_git_commit_worker(&dirty, &worker_active) {
                    break;
                }
            }
        });
    }

    /// Flush a pending vault snapshot before the process exits — bounded.
    ///
    /// At shutdown the coalesced worker may be inside `git add -A`/`git commit`
    /// or still in its debounce sleep. Killing the process on top of it is what
    /// leaves orphan loose objects and quarantined `index.lock` files in the
    /// vault, so this waits for the worker instead of racing it:
    ///
    /// - no worker running → take the dirty flag and run the final snapshot
    ///   synchronously ([`VaultFlush::Committed`]);
    /// - worker running → wait up to `wait` for its own pass to finish, which
    ///   covers every write marked dirty so far ([`VaultFlush::DrainedByWorker`]);
    /// - worker still running at the deadline → start nothing
    ///   ([`VaultFlush::WorkerBusy`]): a second `git add -A` next to a live one
    ///   would fight over `index.lock`.
    pub async fn flush_pending_commit(&self, wait: Duration) -> VaultFlush {
        if !self.auto_git_commit {
            return VaultFlush::Idle;
        }
        if self.git_commit_worker_active.load(Ordering::Acquire) {
            let deadline = tokio::time::Instant::now() + wait;
            while self.git_commit_worker_active.load(Ordering::Acquire) {
                if tokio::time::Instant::now() >= deadline {
                    return VaultFlush::WorkerBusy;
                }
                tokio::time::sleep(FLUSH_POLL_INTERVAL).await;
            }
            return VaultFlush::DrainedByWorker;
        }
        // No worker: the flush owns the repository. Clearing the flag first
        // means a write arriving now re-arms the worker for a normal pass.
        if !self.git_commit_dirty.swap(false, Ordering::AcqRel) {
            return VaultFlush::Idle;
        }
        let root = self.root.clone();
        let author = self.git_author.clone();
        match tokio::task::spawn_blocking(move || run_vault_git_commit(&root, &author)).await {
            Ok(push) => VaultFlush::Committed(push),
            Err(error) => VaultFlush::Failed(error.to_string()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    Kb,
    Episodic,
}

// ─────────────────────────────── frontmatter ───────────────────────────────

/// Parse YAML frontmatter: returns `(metadata, body)`.
pub fn frontmatter_parse(text: &str) -> (Option<Map<String, Value>>, String) {
    let Some(rest) = text.strip_prefix("---") else {
        return (None, text.trim().to_string());
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text.trim().to_string());
    };
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim().to_string();
    match serde_yaml::from_str::<Map<String, Value>>(yaml) {
        Ok(meta) => (Some(meta), body),
        Err(_) => (None, text.trim().to_string()),
    }
}

/// Render a document as frontmatter + body.
pub fn render_doc(doc: &Document) -> SlcResult<String> {
    let mut meta = Map::new();
    meta.insert("id".into(), json!(doc.document_id));
    meta.insert("category".into(), json!(doc.category.as_str()));
    meta.insert("content_hash".into(), json!(doc.content_hash));
    meta.insert("metadata".into(), serde_json::to_value(&doc.metadata)?);
    meta.insert("tags".into(), json!(doc.tags));
    meta.insert("auto_load".into(), json!(doc.auto_load));
    meta.insert("references".into(), json!(doc.references));
    meta.insert("seat_id".into(), json!(doc.seat_id));
    meta.insert("created_at".into(), json!(doc.created_at.to_rfc3339()));
    meta.insert("updated_at".into(), json!(doc.updated_at.to_rfc3339()));
    meta.insert("version".into(), json!(doc.version));
    meta.insert(
        "deleted_at".into(),
        json!(doc.deleted_at.map(|d| d.to_rfc3339())),
    );
    let yaml = serde_yaml::to_string(&meta).map_err(|e| SlcError::Parse(e.to_string()))?;
    Ok(format!("---\n{yaml}---\n\n{}\n", doc.content))
}

// ─────────────────────────────── StorageBackend ───────────────────────────────

#[async_trait]
impl StorageBackend for ObsidianVaultStore {
    async fn kb_insert(&self, doc: &Document) -> SlcResult<()> {
        if !doc.category.is_kb() {
            return Err(SlcError::Storage(
                "history docs go to the episodic store, not the KB".into(),
            ));
        }
        if self.index.lock().unwrap().contains_key(&doc.document_id) {
            return Err(SlcError::Storage(format!(
                "document already exists: {}",
                doc.document_id
            )));
        }
        let doc = doc.clone();
        self.write_note(&doc)?;
        self.git_commit();
        Ok(())
    }

    async fn kb_get(&self, document_id: &str) -> SlcResult<Option<Document>> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        match entry {
            Some(e) if e.deleted_at.is_none() => Ok(Some(self.read_note(&e)?)),
            _ => Ok(None),
        }
    }

    async fn kb_update_content(&self, document_id: &str, content: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        let Some(mut doc) = entry.map(|e| self.read_note(&e)).transpose()? else {
            return Ok(false);
        };
        doc.content = content.to_string();
        doc.content_hash = content_hash(content);
        doc.version += 1;
        doc.updated_at = Utc::now();
        self.write_note(&doc)?;
        self.git_commit();
        Ok(true)
    }

    async fn kb_upsert(&self, doc: &Document) -> SlcResult<bool> {
        let exists = self.index.lock().unwrap().contains_key(&doc.document_id);
        if exists {
            self.kb_replace(doc).await?;
            return Ok(true);
        }
        self.kb_insert(doc).await?;
        Ok(false)
    }

    async fn kb_replace(&self, doc: &Document) -> SlcResult<bool> {
        if !doc.category.is_kb() {
            return Err(SlcError::Storage(
                "history docs go to the episodic store, not the KB".into(),
            ));
        }
        let exists = self.index.lock().unwrap().contains_key(&doc.document_id);
        let doc = doc.clone();
        self.write_note(&doc)?;
        self.git_commit();
        Ok(exists)
    }

    async fn kb_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>> {
        let entries = self.select(Table::Kb, filter, sort, limit);
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            out.push(self.read_note(&e)?);
        }
        Ok(out)
    }

    async fn kb_find_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<Document>> {
        let entries = self.select_unbounded(Table::Kb, filter, sort);
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            out.push(self.read_note(&entry)?);
        }
        Ok(out)
    }

    async fn kb_find_metadata_unbounded(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
    ) -> SlcResult<Vec<KbMetadataProjection>> {
        Ok(self
            .select_unbounded(Table::Kb, filter, sort)
            .into_iter()
            .filter_map(|entry| {
                DocumentCategory::parse(&entry.category).map(|category| KbMetadataProjection {
                    document_id: entry.id,
                    category,
                    seat_id: entry.seat_id,
                    metadata_extra: entry.metadata_extra,
                })
            })
            .collect())
    }

    async fn kb_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        Ok(self
            .select(Table::Kb, filter, &DocSort::default(), usize::MAX)
            .len() as u64)
    }

    async fn kb_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        let entries = self.select(Table::Kb, filter, &DocSort::default(), usize::MAX);
        let mut changed = 0;
        for e in entries {
            let mut doc = self.read_note(&e)?;
            let mut m = doc.metadata.clone();
            if let Some(a) = patch.set_archived {
                m.archived = Some(a);
            }
            if let Some(c) = patch.set_consolidated {
                m.consolidated = Some(c);
            }
            if let Some(b) = &patch.set_compression_batch_id {
                m.compression_batch_id = Some(b.clone());
            }
            doc.metadata = m;
            doc.updated_at = Utc::now();
            self.write_note(&doc)?;
            changed += 1;
        }
        self.git_commit();
        Ok(changed)
    }

    async fn kb_soft_delete(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        let Some(mut doc) = entry.map(|e| self.read_note(&e)).transpose()? else {
            return Ok(false);
        };
        if doc.deleted_at.is_some() {
            return Ok(false);
        }
        doc.deleted_at = Some(Utc::now());
        doc.version += 1;
        self.write_note(&doc)?;
        self.git_commit();
        Ok(true)
    }

    async fn kb_restore(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().get(document_id).cloned();
        let Some(mut doc) = entry.map(|e| self.read_note(&e)).transpose()? else {
            return Ok(false);
        };
        if doc.deleted_at.is_none() {
            return Ok(false);
        }
        doc.deleted_at = None;
        doc.version += 1;
        self.write_note(&doc)?;
        self.git_commit();
        Ok(true)
    }

    async fn kb_replace_many(&self, docs: &[Document]) -> SlcResult<()> {
        for doc in docs {
            if !doc.category.is_kb() {
                return Err(SlcError::Storage(
                    "history docs go to the episodic store, not the KB".into(),
                ));
            }
            let doc = doc.clone();
            self.write_note(&doc)?;
        }
        self.git_commit();
        Ok(())
    }

    async fn kb_rename(&self, old_id: &str, new_id: &str) -> SlcResult<bool> {
        // All work with the guard stays in a block without await (the future must be Send).
        let renamed = {
            let mut index = self.index.lock().unwrap();
            let Some(entry) = index.get(old_id).cloned() else {
                return Ok(false);
            };
            if index.contains_key(new_id) {
                return Err(SlcError::Storage(format!(
                    "document already exists: {new_id}"
                )));
            }
            let dir = self.root.join(sanitize_folder(&entry.folder)?);
            let old_path = dir.join(format!("{}.md", safe_file_name(old_id)));
            let new_path = dir.join(format!("{}.md", safe_file_name(new_id)));
            if !old_path.is_file() {
                return Ok(false);
            }
            std::fs::rename(&old_path, &new_path)?;
            index.remove(old_id);
            let mut e = entry;
            e.id = new_id.to_string();
            index.insert(new_id.to_string(), e);
            true
        };
        if renamed {
            // The note carries the id in its frontmatter (`render_doc`), so the
            // file move alone leaves `id: <old>` inside the note: the next index
            // rebuild — a restart, the engine's `refresh`, any other service
            // sharing the vault — would re-register the OLD id and resolve it to
            // a path that no longer exists. Rewrite the moved note under its new
            // id; the rename stays a move, not an edit (version/updated_at are
            // carried over untouched).
            let entry = self.index.lock().unwrap().get(new_id).cloned();
            if let Some(mut entry) = entry {
                // The moved file is named after the new id, never after the
                // previous entry's file name.
                entry.file_name = None;
                let mut doc = self.read_note(&entry)?;
                doc.document_id = new_id.to_string();
                let path = self
                    .root
                    .join(sanitize_folder(&entry.folder)?)
                    .join(format!("{}.md", safe_file_name(new_id)));
                self.atomic_write(&path, render_doc(&doc)?.as_bytes())?;
            }
        }
        self.persist_index()?;
        self.git_commit();
        Ok(renamed)
    }

    async fn kb_purge(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().remove(document_id);
        let Some(e) = entry else { return Ok(false) };
        let path = self
            .root
            .join(&sanitize_folder(&e.folder)?)
            .join(format!("{}.md", safe_file_name(&e.id)));
        let _ = tokio::fs::remove_file(path).await;
        self.delete_embeddings(document_id).await?;
        self.persist_index()?;
        self.git_commit();
        Ok(true)
    }

    async fn kb_graveyard(&self, days: Option<i64>) -> SlcResult<Vec<Document>> {
        let cutoff = days.map(|d| Utc::now() - chrono::Duration::days(d));
        let entries = self.select(
            Table::Kb,
            &DocFilter {
                deleted: true,
                ..Default::default()
            },
            &DocSort::default(),
            usize::MAX,
        );
        let mut out = Vec::new();
        for e in entries {
            let doc = self.read_note(&e)?;
            if let Some(deleted) = doc.deleted_at {
                if cutoff.map_or(true, |c| deleted <= c) {
                    out.push(doc);
                }
            }
        }
        Ok(out)
    }

    async fn kb_cleanup_graveyard(&self, days: i64) -> SlcResult<u64> {
        let graveyard = self.kb_graveyard(Some(days)).await?;
        let mut n = 0;
        for doc in graveyard {
            if self.kb_purge(&doc.document_id).await? {
                n += 1;
            }
        }
        Ok(n)
    }

    // ── episodic ────────────────────────────────────────────────

    async fn episodic_insert(&self, doc: &Document) -> SlcResult<()> {
        if doc.category != DocumentCategory::History {
            return Err(SlcError::Storage(
                "only history docs go to the episodic store".into(),
            ));
        }
        // Seat-less episodic docs are allowed (legacy import): they are not
        // picked up by any per-seat pipeline, just stored as diary.
        if self.index.lock().unwrap().contains_key(&doc.document_id) {
            return Err(SlcError::Storage(format!(
                "document already exists: {}",
                doc.document_id
            )));
        }
        let doc = doc.clone();
        self.write_note(&doc)?;
        self.git_commit();
        Ok(())
    }

    async fn episodic_upsert(&self, doc: &Document) -> SlcResult<bool> {
        if doc.category != DocumentCategory::History {
            return Err(SlcError::Storage(
                "only history docs go to the episodic store".into(),
            ));
        }
        if self.index.lock().unwrap().contains_key(&doc.document_id) {
            self.write_note(doc)?;
            self.git_commit();
            return Ok(true);
        }
        self.episodic_insert(doc).await?;
        Ok(false)
    }

    async fn episodic_find(
        &self,
        filter: &DocFilter,
        sort: &DocSort,
        limit: usize,
    ) -> SlcResult<Vec<Document>> {
        let entries = self.select(Table::Episodic, filter, sort, limit);
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            out.push(self.read_note(&e)?);
        }
        Ok(out)
    }

    async fn episodic_count(&self, filter: &DocFilter) -> SlcResult<u64> {
        Ok(self
            .select(Table::Episodic, filter, &DocSort::default(), usize::MAX)
            .len() as u64)
    }

    async fn episodic_patch_meta(&self, filter: &DocFilter, patch: &MetaPatch) -> SlcResult<usize> {
        let entries = self.select(Table::Episodic, filter, &DocSort::default(), usize::MAX);
        let mut changed = 0;
        for e in entries {
            let mut doc = self.read_note(&e)?;
            let mut m = doc.metadata.clone();
            if let Some(a) = patch.set_archived {
                m.archived = Some(a);
            }
            if let Some(c) = patch.set_consolidated {
                m.consolidated = Some(c);
            }
            if let Some(b) = &patch.set_compression_batch_id {
                m.compression_batch_id = Some(b.clone());
            }
            doc.metadata = m;
            doc.updated_at = Utc::now();
            self.write_note(&doc)?;
            changed += 1;
        }
        self.git_commit();
        Ok(changed)
    }

    async fn episodic_purge(&self, document_id: &str) -> SlcResult<bool> {
        let entry = self.index.lock().unwrap().remove(document_id);
        let Some(e) = entry else { return Ok(false) };
        let path = self
            .root
            .join(&sanitize_folder(&e.folder)?)
            .join(format!("{}.md", safe_file_name(&e.id)));
        let _ = tokio::fs::remove_file(path).await;
        self.persist_index()?;
        self.git_commit();
        Ok(true)
    }

    // ── embeddings ──────────────────────────────────────────────

    async fn insert_embeddings(&self, records: &[EmbeddingRecord]) -> SlcResult<()> {
        {
            let mut map = self.embeddings.lock().unwrap();
            for r in records {
                map.entry(r.document_id.clone())
                    .or_default()
                    .push(r.clone());
            }
        }
        self.persist_embeddings()?;
        self.git_commit();
        Ok(())
    }

    async fn get_embedding(&self, document_id: &str) -> SlcResult<Option<Vec<f32>>> {
        let map = self.embeddings.lock().unwrap();
        Ok(map
            .get(document_id)
            .and_then(|chunks| chunks.first())
            .map(|c| c.embedding.clone()))
    }

    async fn get_all_chunks(&self, document_id: &str) -> SlcResult<Vec<EmbeddingRecord>> {
        let map = self.embeddings.lock().unwrap();
        Ok(map.get(document_id).cloned().unwrap_or_default())
    }

    async fn all_embeddings(
        &self,
        scope: EmbeddingScope,
        seat_id: Option<&str>,
    ) -> SlcResult<Vec<EmbeddingRecord>> {
        let map = self.embeddings.lock().unwrap();
        let mut out = Vec::new();
        for chunks in map.values() {
            for c in chunks {
                if c.scope != scope {
                    continue;
                }
                if let Some(seat) = seat_id {
                    if c.seat_id.as_deref() != Some(seat) {
                        continue;
                    }
                }
                out.push(c.clone());
            }
        }
        Ok(out)
    }

    async fn delete_embeddings(&self, document_id: &str) -> SlcResult<()> {
        self.embeddings.lock().unwrap().remove(document_id);
        self.persist_embeddings()?;
        self.git_commit();
        Ok(())
    }

    /// One map mutation + ONE `embeddings.json` rewrite for the whole batch.
    ///
    /// `persist_embeddings` re-serializes the complete in-memory map, so the
    /// default body (delete + insert per document) rewrites the file 2N times
    /// for a batch of N — quadratic. On a vault whose map is ~80 MB that turns a
    /// stale-embedding refresh into a process that burns one core for hours
    /// without ever finishing (the CLI `search` / `reindex-embeddings` path).
    async fn replace_embeddings(
        &self,
        replacements: &[(String, EmbeddingRecord)],
    ) -> SlcResult<()> {
        if replacements.is_empty() {
            return Ok(());
        }
        {
            let mut map = self.embeddings.lock().unwrap();
            for (document_id, record) in replacements {
                // `insert`, not `entry().or_default().push()`: replacing drops
                // the previous chunks and keeps exactly the record handed in.
                map.insert(document_id.clone(), vec![record.clone()]);
            }
        }
        self.persist_embeddings()?;
        self.git_commit();
        Ok(())
    }

    // ── seats ───────────────────────────────────────────────────

    async fn insert_seat(&self, seat: &Seat) -> SlcResult<()> {
        self.seats
            .lock()
            .unwrap()
            .insert(seat.seat_id.clone(), seat.clone());
        self.persist_seat(seat)?;
        self.git_commit();
        Ok(())
    }

    async fn get_seat(&self, seat_id: &str) -> SlcResult<Option<Seat>> {
        Ok(self.seats.lock().unwrap().get(seat_id).cloned())
    }

    async fn list_active_seats(&self, limit: usize) -> SlcResult<Vec<Seat>> {
        let mut seats: Vec<Seat> = self
            .seats
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.status == SeatStatus::Active)
            .cloned()
            .collect();
        seats.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));
        seats.truncate(limit);
        Ok(seats)
    }

    async fn touch_seat(&self, seat_id: &str) -> SlcResult<bool> {
        let seat = {
            let mut seats = self.seats.lock().unwrap();
            let Some(seat) = seats.get_mut(seat_id) else {
                return Ok(false);
            };
            let now = Utc::now();
            // Authentication touches every tool call. Persisting and pushing
            // each one makes a read-only request unexpectedly expensive, so
            // retain a useful heartbeat while coalescing that volatile field.
            if now.signed_duration_since(seat.last_accessed) < chrono::Duration::minutes(1) {
                return Ok(true);
            }
            seat.last_accessed = now;
            seat.clone()
        };
        self.persist_seat(&seat)?;
        self.git_commit();
        Ok(true)
    }

    async fn set_seat_status(&self, seat_id: &str, status: SeatStatus) -> SlcResult<bool> {
        let seat = {
            let mut seats = self.seats.lock().unwrap();
            let Some(seat) = seats.get_mut(seat_id) else {
                return Ok(false);
            };
            seat.status = status;
            seat.clone()
        };
        self.persist_seat(&seat)?;
        self.git_commit();
        Ok(true)
    }

    async fn set_seat_active_task(&self, seat_id: &str, task_id: &str) -> SlcResult<bool> {
        let seat = {
            let mut seats = self.seats.lock().unwrap();
            let Some(seat) = seats.get_mut(seat_id) else {
                return Ok(false);
            };
            seat.active_task_id = Some(task_id.into());
            // Task activation is document activation too (unified anchor).
            seat.active_document_id = Some(task_id.into());
            seat.clone()
        };
        self.persist_seat(&seat)?;
        self.git_commit();
        Ok(true)
    }

    async fn set_seat_active_document(
        &self,
        seat_id: &str,
        document_id: Option<&str>,
    ) -> SlcResult<bool> {
        let seat = {
            let mut seats = self.seats.lock().unwrap();
            let Some(seat) = seats.get_mut(seat_id) else {
                return Ok(false);
            };
            seat.active_document_id = document_id.map(String::from);
            seat.clone()
        };
        self.persist_seat(&seat)?;
        self.git_commit();
        Ok(true)
    }

    async fn get_seat_active_document(&self, seat_id: &str) -> SlcResult<Option<String>> {
        let seats = self.seats.lock().unwrap();
        let Some(seat) = seats.get(seat_id) else {
            return Ok(None);
        };
        // Unified field first; fall back to the legacy task pointer.
        Ok(seat
            .active_document_id
            .clone()
            .or_else(|| seat.active_task_id.clone()))
    }

    async fn incr_seat_stats(
        &self,
        seat_id: &str,
        tool_name: &str,
        tokens_used: i64,
    ) -> SlcResult<bool> {
        let seat = {
            let mut seats = self.seats.lock().unwrap();
            let Some(seat) = seats.get_mut(seat_id) else {
                return Ok(false);
            };
            seat.usage_stats.total_requests += 1;
            seat.usage_stats.total_tokens += tokens_used;
            let count = seat
                .usage_stats
                .tools_used
                .get(tool_name)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                + 1;
            seat.usage_stats
                .tools_used
                .insert(tool_name.into(), json!(count));
            seat.last_accessed = Utc::now();
            seat.clone()
        };
        self.persist_seat(&seat)?;
        self.git_commit();
        Ok(true)
    }

    // ── timers ──────────────────────────────────────────────────

    async fn insert_timer(&self, timer: &PersistedTimer) -> SlcResult<()> {
        self.timers
            .lock()
            .unwrap()
            .insert(timer.timer_id.clone(), timer.clone());
        self.persist_timers()?;
        self.git_commit();
        Ok(())
    }

    async fn get_timer(&self, timer_id: &str) -> SlcResult<Option<PersistedTimer>> {
        Ok(self.timers.lock().unwrap().get(timer_id).cloned())
    }

    async fn active_timers(&self, seat_id: Option<&str>) -> SlcResult<Vec<PersistedTimer>> {
        let timers = self.timers.lock().unwrap();
        let mut out: Vec<PersistedTimer> = timers
            .values()
            .filter(|t| t.is_active && seat_id.map_or(true, |s| t.seat_id == s))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.next_fire_at.cmp(&b.next_fire_at));
        Ok(out)
    }

    async fn set_timer_fired(&self, timer_id: &str, now: DateTime<Utc>) -> SlcResult<()> {
        {
            let mut timers = self.timers.lock().unwrap();
            if let Some(t) = timers.get_mut(timer_id) {
                t.last_fired_at = Some(now);
            }
        }
        self.persist_timers()?;
        self.git_commit();
        Ok(())
    }

    // ── records ─────────────────────────────────────────────────

    async fn put_record(&self, collection: &str, key: &str, value: &Value) -> SlcResult<()> {
        self.records
            .lock()
            .unwrap()
            .insert((collection.to_string(), key.to_string()), value.clone());
        self.persist_records()?;
        self.git_commit();
        Ok(())
    }

    async fn get_record(&self, collection: &str, key: &str) -> SlcResult<Option<Value>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .get(&(collection.to_string(), key.to_string()))
            .cloned())
    }

    async fn delete_record(&self, collection: &str, key: &str) -> SlcResult<bool> {
        let existed = self
            .records
            .lock()
            .unwrap()
            .remove(&(collection.to_string(), key.to_string()))
            .is_some();
        if existed {
            self.persist_records()?;
            self.git_commit();
        }
        Ok(existed)
    }

    async fn list_records(&self, collection: &str) -> SlcResult<Vec<(String, Value)>> {
        let records = self.records.lock().unwrap();
        let mut out: Vec<(String, Value)> = records
            .iter()
            .filter(|((c, _), _)| c == collection)
            .map(|((_, k), v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    async fn health_check(&self) -> bool {
        self.root.exists()
    }

    /// Multi-process sync: re-scan the vault from disk (documents, seats,
    /// sidecars). Files are the source of truth — picks up changes made by
    /// another service (e.g. the web UI) sharing the same vault.
    async fn refresh(&self) -> SlcResult<()> {
        self.rebuild_index()?;
        self.load_sidecars()?;
        Ok(())
    }

    async fn close(&self) -> SlcResult<()> {
        Ok(())
    }

    /// See [`ObsidianVaultStore::flush_pending_commit`]: the shutdown path must
    /// not exit on top of the coalesced Git worker (that is what leaves orphan
    /// objects in the vault), but it must also never hang — the bound is
    /// honoured and the outcome is logged for the operator.
    async fn flush_pending(&self, wait: Duration) -> SlcResult<()> {
        match self.flush_pending_commit(wait).await {
            VaultFlush::Idle => Ok(()),
            VaultFlush::Committed(push) => {
                tracing::info!("vault flush: pending snapshot committed ({push:?})");
                Ok(())
            }
            VaultFlush::DrainedByWorker => {
                tracing::info!("vault flush: coalesced worker finished the pending snapshot");
                Ok(())
            }
            VaultFlush::WorkerBusy => {
                tracing::warn!(
                    "vault flush: coalesced snapshot worker still running after {wait:?}; \
                     leaving the in-flight snapshot to finish"
                );
                Ok(())
            }
            VaultFlush::Failed(error) => {
                tracing::warn!("vault flush failed: {error}");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DocLevel, DocMeta};
    use chrono::Datelike;

    fn tmp_vault(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("slc-test-{tag}-{}", uuid::Uuid::new_v4().simple()))
    }

    /// Run git in `dir`, asserting success, and return stdout. Vault snapshots
    /// are a Git feature, so a missing/broken Git must fail loudly here instead
    /// of silently skipping the behaviour under test.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git must be available for vault git tests");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A vault repository with a deterministic committer identity (the snapshot
    /// worker sets author/committer only for the author fields).
    fn git_vault(tag: &str) -> PathBuf {
        let root = tmp_vault(tag);
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.name", "SLC Test"]);
        git(&root, &["config", "user.email", "slc-test@localhost"]);
        root
    }

    fn commit_count(root: &Path) -> usize {
        git(root, &["rev-list", "--count", "HEAD"])
            .parse()
            .expect("rev-list count")
    }

    /// A plain public KB document in the vault root.
    fn doc_for(id: &str) -> Document {
        Document::new(
            id,
            DocumentCategory::Documentation,
            format!("body of {id}"),
            DocMeta::default(),
            vec![],
            None,
        )
    }

    /// A long id must survive the filesystem, the note frontmatter, and an
    /// index rebuild: the stored id stays byte-identical, and the truncated
    /// prefix a caller might reconstruct is not silently accepted as a key.
    #[tokio::test]
    async fn long_ids_survive_the_vault_filesystem_and_a_rescan() {
        let root = tmp_vault("long-id");
        let store = ObsidianVaultStore::open(&root, false).unwrap();
        let long_id = "diag_disposable_contactor_roy1_probe_20260915t042530";
        assert!(long_id.chars().count() > 48);
        let doc = Document::with_folder(
            long_id,
            DocumentCategory::Task,
            Some("tasks".into()),
            "marker body",
            DocMeta::default(),
            vec![],
            Some("seat-contactor".into()),
        );
        store.kb_insert(&doc).await.unwrap();
        // The note file keeps the whole id (no truncated stem).
        assert!(root.join("tasks").join(format!("{long_id}.md")).exists());

        // Fresh open = index rebuilt from disk: the exact id still resolves.
        let reopened = ObsidianVaultStore::open(&root, false).unwrap();
        let loaded = reopened
            .kb_get(long_id)
            .await
            .unwrap()
            .expect("the exact stored id must resolve");
        assert_eq!(loaded.document_id, long_id);
        let visible = reopened
            .kb_find(
                &DocFilter::visible_to("seat-contactor"),
                &DocSort::by_updated(SortDir::Desc),
                10,
            )
            .await
            .unwrap();
        assert_eq!(visible[0].document_id, long_id);

        // The 48-char prefix is a different key: a truncated request misses
        // instead of silently resolving to some other note.
        let prefix: String = long_id.chars().take(48).collect();
        assert!(reopened.kb_get(&prefix).await.unwrap().is_none());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vault_remote_configured_only_when_a_remote_exists() {
        let root = git_vault("remote-detect");
        assert!(
            !vault_remote_configured(&root),
            "a fresh vault has no remote"
        );
        let upstream = tmp_vault("remote-detect-upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q", "--bare"]);
        git(
            &root,
            &["remote", "add", "origin", upstream.to_str().unwrap()],
        );
        assert!(
            vault_remote_configured(&root),
            "a configured remote must be detected"
        );
        // A non-repository directory is not a push target either.
        let plain = tmp_vault("remote-detect-plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(!vault_remote_configured(&plain));
        for path in [root, upstream, plain] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// The defect: an unconditional `git push -q` failed on every auto-commit in
    /// a vault with no remote. The snapshot must still commit, and must not run
    /// a push process at all.
    #[test]
    fn vault_commit_skips_push_and_stays_quiet_when_no_remote_is_configured() {
        let root = git_vault("push-no-remote");
        std::fs::write(root.join("marker.md"), "first\n").unwrap();

        let first = run_vault_git_commit(&root, "SLC Test <slc-test@localhost>");
        assert_eq!(first, VaultPush::SkippedNoRemote, "{first:?}");
        assert_eq!(commit_count(&root), 1, "the vault must still auto-commit");
        assert_eq!(git(&root, &["remote"]), "", "no remote is configured");

        // Every later snapshot takes the same quiet path — no failed push, no
        // new journal line, HEAD advances.
        std::fs::write(root.join("marker.md"), "second\n").unwrap();
        let second = run_vault_git_commit(&root, "SLC Test <slc-test@localhost>");
        assert_eq!(second, VaultPush::SkippedNoRemote, "{second:?}");
        assert_eq!(commit_count(&root), 2, "HEAD must advance per snapshot");

        // An unchanged vault manufactures no commit at all.
        let third = run_vault_git_commit(&root, "SLC Test <slc-test@localhost>");
        assert_eq!(third, VaultPush::NoCommit, "{third:?}");
        assert_eq!(commit_count(&root), 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    /// The same code path must still push when a remote exists — the fix may not
    /// silently disable vault backup.
    #[test]
    fn vault_commit_pushes_to_a_configured_remote() {
        let root = git_vault("push-remote");
        let upstream = tmp_vault("push-remote-upstream");
        std::fs::create_dir_all(&upstream).unwrap();
        git(&upstream, &["init", "-q", "--bare"]);

        std::fs::write(root.join("marker.md"), "initial\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        git(
            &root,
            &["remote", "add", "origin", upstream.to_str().unwrap()],
        );
        git(&root, &["push", "-q", "-u", "origin", "HEAD"]);

        std::fs::write(root.join("marker.md"), "snapshot\n").unwrap();
        let outcome = run_vault_git_commit(&root, "SLC Test <slc-test@localhost>");
        assert_eq!(outcome, VaultPush::Pushed, "{outcome:?}");

        // Proof that the push really happened: the upstream now carries the
        // snapshot commit the worker created.
        let upstream_subject = git(&upstream, &["log", "-1", "--format=%s"]);
        assert_eq!(upstream_subject, "slc: vault update");
        assert_eq!(
            git(&root, &["rev-parse", "HEAD"]),
            git(&upstream, &["rev-parse", "HEAD"])
        );

        for path in [root, upstream] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[tokio::test]
    async fn vault_layout_and_names() {
        let root = tmp_vault("layout");
        let store = ObsidianVaultStore::open(&root, false).unwrap();

        // KB doc with explicit human folder + name.
        let mut m = DocMeta::default();
        m.doc_type = Some("plan".into());
        let doc = Document::with_folder(
            "vassista-phase-1",
            DocumentCategory::Project,
            Some("projects/vassista".into()),
            "# Phase 1 plan\n\n- audio half",
            m,
            vec!["priority:high".into()],
            None,
        );
        store.kb_insert(&doc).await.unwrap();

        let path = root.join("projects/vassista/vassista-phase-1.md");
        assert!(path.exists(), "expected note at {path:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("id: vassista-phase-1"));
        assert!(text.contains("# Phase 1 plan"));

        // Roundtrip via fresh open (hand-edit simulation: index rebuilt).
        let store2 = ObsidianVaultStore::open(&root, false).unwrap();
        let loaded = store2.kb_get("vassista-phase-1").await.unwrap().unwrap();
        assert_eq!(loaded.content, "# Phase 1 plan\n\n- audio half");
        assert_eq!(loaded.metadata.doc_type.as_deref(), Some("plan"));

        // History goes to history/YYYY/MM/ and is invisible to KB search.
        let mut hm = DocMeta::default();
        hm.doc_level = Some(DocLevel::L1);
        hm.seat_id = Some("seat_1".into());
        let evt = Document::new(
            "session-2026-08-13-morning",
            DocumentCategory::History,
            "worked on the STT pipeline",
            hm,
            vec![],
            Some("seat_1".into()),
        );
        store2.episodic_insert(&evt).await.unwrap();
        let rel = root
            .join("history")
            .join(format!("{:04}", Utc::now().year()))
            .join(format!("{:02}", Utc::now().month()));
        assert!(rel.join("session-2026-08-13-morning.md").exists());

        let kb_count = store2.kb_count(&DocFilter::default()).await.unwrap();
        assert_eq!(kb_count, 1, "history must not be in KB");
        let ep_count = store2.episodic_count(&DocFilter::default()).await.unwrap();
        assert_eq!(ep_count, 1);
        assert!(
            store2.kb_insert(&evt).await.is_err(),
            "history rejected by kb_insert"
        );
    }

    #[tokio::test]
    async fn upsert_replaces_metadata_and_removes_the_old_note_path() {
        let root = tmp_vault("upsert-move");
        let store = ObsidianVaultStore::open(&root, false).unwrap();
        let original = Document::with_folder(
            "same-id",
            DocumentCategory::Documentation,
            Some("docs/legacy".into()),
            "old content",
            DocMeta::default(),
            vec!["old".into()],
            None,
        );
        store.kb_insert(&original).await.unwrap();

        let mut metadata = DocMeta::default();
        metadata.extra.insert("status".into(), json!("IN_WORK"));
        let replacement = Document::with_folder(
            "same-id",
            DocumentCategory::Task,
            Some("docs/projects/demo/tasks".into()),
            "new content",
            metadata,
            vec!["new".into()],
            Some("seat-a".into()),
        );
        assert!(store.kb_upsert(&replacement).await.unwrap());
        assert!(!root.join("docs/legacy/same-id.md").exists());
        assert!(root.join("docs/projects/demo/tasks/same-id.md").exists());

        let reopened = ObsidianVaultStore::open(&root, false).unwrap();
        let loaded = reopened.kb_get("same-id").await.unwrap().unwrap();
        assert_eq!(loaded.category, DocumentCategory::Task);
        assert_eq!(loaded.content, "new content");
        assert_eq!(loaded.tags, vec!["new"]);
        assert_eq!(loaded.seat_id.as_deref(), Some("seat-a"));
        assert_eq!(loaded.metadata.extra["status"], "IN_WORK");
    }

    #[tokio::test]
    async fn flattened_extra_metadata_filters_survive_index_rebuild() {
        let root = tmp_vault("extra-filter");
        let store = ObsidianVaultStore::open(&root, false).unwrap();

        for (id, assignee, status) in [
            ("task-a", "developer-0", "RUNNING"),
            ("task-b", "developer-0", "QUEUED"),
            ("task-c", "developer-1", "QUEUED"),
        ] {
            let mut metadata = DocMeta::default();
            metadata.extra.insert("assignee".into(), json!(assignee));
            metadata.extra.insert("status".into(), json!(status));
            store
                .kb_insert(&Document::new(
                    id,
                    DocumentCategory::Task,
                    id,
                    metadata,
                    vec![],
                    Some("manager".into()),
                ))
                .await
                .unwrap();
        }

        let mut filter = DocFilter::default();
        filter
            .extra_strings
            .insert("assignee".into(), "developer-0".into());
        filter
            .extra_strings_not_in
            .insert("status".into(), vec!["RUNNING".into()]);
        let found = store
            .kb_find(&filter, &DocSort::default(), 10)
            .await
            .unwrap();
        assert_eq!(
            found
                .iter()
                .map(|doc| doc.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-b"]
        );

        let reopened = ObsidianVaultStore::open(&root, false).unwrap();
        let found_after_rebuild = reopened
            .kb_find(&filter, &DocSort::default(), 10)
            .await
            .unwrap();
        assert_eq!(
            found_after_rebuild
                .iter()
                .map(|doc| doc.document_id.as_str())
                .collect::<Vec<_>>(),
            vec!["task-b"]
        );
    }

    #[tokio::test]
    async fn episodic_compression_queries() {
        let root = tmp_vault("compress");
        let store = ObsidianVaultStore::open(&root, false).unwrap();
        let seat = "seat_c";

        for i in 0..5 {
            let mut m = DocMeta::default();
            m.doc_level = Some(DocLevel::L1);
            m.seat_id = Some(seat.into());
            store
                .episodic_insert(&Document::new(
                    format!("event-{i}"),
                    DocumentCategory::History,
                    format!("event {i}"),
                    m,
                    vec![],
                    Some(seat.into()),
                ))
                .await
                .unwrap();
        }

        let f = DocFilter {
            seat_id: Some(seat.into()),
            doc_level: Some(DocLevel::L1),
            has_compression_batch: Some(false),
            ..Default::default()
        };
        assert_eq!(store.episodic_count(&f).await.unwrap(), 5);

        let patch = MetaPatch {
            set_archived: Some(true),
            set_compression_batch_id: Some("batch_1".into()),
            ..Default::default()
        };
        assert_eq!(store.episodic_patch_meta(&f, &patch).await.unwrap(), 5);
        assert_eq!(
            store
                .episodic_count(&DocFilter {
                    has_compression_batch: Some(false),
                    ..Default::default()
                })
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn embeddings_sidecar() {
        let root = tmp_vault("emb");
        let store = ObsidianVaultStore::open(&root, false).unwrap();
        store
            .insert_embeddings(&[EmbeddingRecord {
                document_id: "doc-a".into(),
                chunk_index: 0,
                chunk_total: 1,
                embedding: vec![0.5, 0.25],
                embedding_model: "bge-m3".into(),
                embedding_dimension: 2,
                generated_at: Utc::now(),
                scope: EmbeddingScope::Public,
                seat_id: None,
            }])
            .await
            .unwrap();
        assert_eq!(
            store.get_embedding("doc-a").await.unwrap(),
            Some(vec![0.5, 0.25])
        );
    }
    #[test]
    fn sanitize_folder_accepts_nested_relative_and_rejects_escapes() {
        // Valid Obsidian layouts.
        assert_eq!(
            sanitize_folder("projects/vassista").unwrap(),
            "projects/vassista"
        );
        assert_eq!(
            sanitize_folder("history/2026/08").unwrap(),
            "history/2026/08"
        );
        // Escapes must be rejected (path traversal / absolute / windows).
        for bad in [
            "../../tmp/x",
            "projects/../..",
            "/etc",
            "a/b/../../c",
            "..",
            "a\\..\\b",
            "a:b",
            "",
            "  ",
        ] {
            assert!(sanitize_folder(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn atomic_replace_preserves_the_inode_seen_by_a_concurrent_reader() {
        use std::io::Read as _;

        let root = tmp_vault("atomic-replace");
        let target = root.join(".slc/index.json");
        let temp_dir = root.join(".git/slc-write-tmp");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"old-complete-value").unwrap();
        let mut existing_reader = std::fs::File::open(&target).unwrap();

        atomic_replace(&temp_dir, &target, b"new-complete-value").unwrap();

        let mut old_value = Vec::new();
        existing_reader.read_to_end(&mut old_value).unwrap();
        assert_eq!(old_value, b"old-complete-value");
        assert_eq!(std::fs::read(&target).unwrap(), b"new-complete-value");
        assert_eq!(std::fs::read_dir(&temp_dir).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_git_index_lock_is_quarantined_but_fresh_lock_is_preserved() {
        let root = tmp_vault("git-lock-recovery");
        let git_dir = root.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let lock = git_dir.join("index.lock");
        std::fs::write(&lock, b"stale evidence").unwrap();
        let modified = std::fs::metadata(&lock).unwrap().modified().unwrap();

        assert!(
            quarantine_stale_git_index_lock(
                &root,
                modified + GIT_INDEX_LOCK_STALE_AFTER - Duration::from_secs(1),
            )
            .unwrap()
            .is_none()
        );
        assert!(lock.exists());

        let quarantined = quarantine_stale_git_index_lock(
            &root,
            modified + GIT_INDEX_LOCK_STALE_AFTER + Duration::from_secs(1),
        )
        .unwrap()
        .expect("stale lock must be quarantined");
        assert!(!lock.exists());
        assert_eq!(std::fs::read(quarantined).unwrap(), b"stale evidence");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn coalesced_git_worker_repeats_only_for_writes_during_a_snapshot() {
        let dirty = AtomicBool::new(true);
        let worker_active = AtomicBool::new(true);

        assert!(finish_git_commit_worker(&dirty, &worker_active));
        assert!(!dirty.load(Ordering::Acquire));
        assert!(worker_active.load(Ordering::Acquire));

        assert!(!finish_git_commit_worker(&dirty, &worker_active));
        assert!(!worker_active.load(Ordering::Acquire));
    }

    /// The shutdown defect: the server was SIGKILLed while a vault snapshot was
    /// still pending, which is what leaves orphan objects in `.git`. A pending
    /// write that the coalesced worker has not picked up yet must be committed
    /// by the flush itself — synchronously, before the process exits.
    #[tokio::test]
    async fn shutdown_flush_commits_a_snapshot_the_worker_has_not_started() {
        let root = git_vault("flush-owns-flag");
        let store = ObsidianVaultStore::open(&root, true).unwrap();
        // The state the shutdown path must resolve: a write is pending, no
        // worker is running. (Set directly instead of through the write path:
        // after a real write the worker is always active immediately, which is
        // the other test below.)
        std::fs::write(root.join("marker.md"), "pending\n").unwrap();
        store.git_commit_dirty.store(true, Ordering::Release);
        assert!(!store.git_commit_worker_active.load(Ordering::Acquire));

        let outcome = store.flush_pending_commit(Duration::from_secs(10)).await;
        assert_eq!(
            outcome,
            VaultFlush::Committed(VaultPush::SkippedNoRemote),
            "{outcome:?}"
        );
        assert_eq!(
            commit_count(&root),
            1,
            "the pending write must reach the repository before exit"
        );
        assert_eq!(
            git(&root, &["status", "--porcelain"]),
            "",
            "no uncommitted work may be left behind"
        );
        assert!(
            !root.join(".git/index.lock").exists(),
            "the flush may not leave a lock behind"
        );

        // Nothing is pending any more: a second flush is idle and must not
        // manufacture an empty commit.
        let again = store.flush_pending_commit(Duration::from_millis(50)).await;
        assert_eq!(again, VaultFlush::Idle, "{again:?}");
        assert_eq!(commit_count(&root), 1);

        std::fs::remove_dir_all(root).unwrap();
    }

    /// A shutdown that lands inside the coalesced worker's debounce window must
    /// wait for that worker, not start a second `git add -A` beside it: two Git
    /// writers fighting over `index.lock` is exactly how the residue appears.
    #[tokio::test]
    async fn shutdown_flush_waits_for_the_coalesced_worker_instead_of_racing_it() {
        let root = git_vault("flush-worker");
        std::fs::write(root.join("marker.md"), "initial\n").unwrap();
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        assert_eq!(commit_count(&root), 1);

        let store = ObsidianVaultStore::open(&root, true).unwrap();
        let doc = Document::new(
            "flush-doc",
            DocumentCategory::Documentation,
            "body written just before the shutdown signal",
            DocMeta::default(),
            vec![],
            None,
        );
        store.kb_insert(&doc).await.unwrap();
        assert!(
            store.git_commit_worker_active.load(Ordering::Acquire),
            "a store write must arm the coalesced worker"
        );
        assert_eq!(
            commit_count(&root),
            1,
            "the snapshot is deferred by the debounce, so it is still pending"
        );

        let started = std::time::Instant::now();
        let outcome = store.flush_pending_commit(Duration::from_secs(30)).await;
        assert!(
            started.elapsed() >= GIT_COMMIT_DEBOUNCE,
            "the flush must wait for the worker's own pass, not cut it short: {:?}",
            started.elapsed()
        );
        assert_eq!(outcome, VaultFlush::DrainedByWorker, "{outcome:?}");
        assert_eq!(
            commit_count(&root),
            2,
            "the write that was pending at shutdown is in the repository"
        );
        assert_eq!(git(&root, &["status", "--porcelain"]), "");
        assert!(
            git(&root, &["ls-tree", "-r", "--name-only", "HEAD"]).contains("flush-doc"),
            "the document itself must be in the committed snapshot"
        );
        assert!(!root.join(".git/index.lock").exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    /// The engine never holds the concrete store — it holds
    /// `Arc<dyn StorageBackend>` — and `flush_pending` has a default body. A
    /// delegation impl that forgets to forward it turns the shutdown flush into
    /// a silent no-op: the process then exits with the pending write still
    /// uncommitted, which is exactly what the 2026-09-15 smoke test on the real
    /// binary showed (checked here through the same trait object).
    #[tokio::test]
    async fn shutdown_flush_survives_the_dyn_delegation_the_engine_uses() {
        let root = git_vault("flush-dyn-delegation");
        let store = std::sync::Arc::new(ObsidianVaultStore::open(&root, true).unwrap());
        // The exact handle shape `SlcEngine` stores.
        let dynamic: std::sync::Arc<dyn StorageBackend> = store.clone();

        std::fs::write(root.join("marker.md"), "pending\n").unwrap();
        store.git_commit_dirty.store(true, Ordering::Release);

        dynamic
            .flush_pending(Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(
            commit_count(&root),
            1,
            "the flush must reach the vault through Arc<dyn StorageBackend>"
        );
        assert_eq!(git(&root, &["status", "--porcelain"]), "");

        std::fs::remove_dir_all(root).unwrap();
    }

    /// A writer that never finishes must not turn the shutdown into a hang: the
    /// flush returns at its deadline and reports that the worker owns the
    /// repository.
    #[tokio::test]
    async fn shutdown_flush_is_bounded_when_the_worker_never_finishes() {
        let root = git_vault("flush-bounded");
        let store = ObsidianVaultStore::open(&root, true).unwrap();
        // A worker that is stuck (a hung `git add -A` on a huge vault).
        store
            .git_commit_worker_active
            .store(true, Ordering::Release);

        let bound = Duration::from_millis(150);
        let started = std::time::Instant::now();
        let outcome = store.flush_pending_commit(bound).await;
        let elapsed = started.elapsed();
        assert_eq!(outcome, VaultFlush::WorkerBusy, "{outcome:?}");
        assert!(
            elapsed >= bound && elapsed < bound + Duration::from_secs(5),
            "the flush must honour its bound: {elapsed:?}"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_git_loose_object_summary_for_maintenance_thresholds() {
        let summary = "count: 2148\nsize: 574904\nin-pack: 2891\nsize-pack: 160460\n";
        assert_eq!(parse_git_count_objects(summary), Some((2148, 574904)));
        assert_eq!(parse_git_count_objects("count: nope\nsize: 1\n"), None);
    }

    #[test]
    fn derived_sidecar_ignore_rules_are_idempotent_and_preserve_existing_rules() {
        let root = tmp_vault("derived-ignore");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "core\n").unwrap();

        ensure_derived_sidecars_ignored(&root).unwrap();
        ensure_derived_sidecars_ignored(&root).unwrap();

        let ignored = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(ignored.lines().any(|line| line == "core"));
        assert_eq!(ignored.matches("/.slc/index.json").count(), 1);
        assert_eq!(ignored.matches("/.slc/embeddings.json").count(), 1);
        // records.json is rewritten on nearly every write; versioning it grew
        // vault history by gigabytes per day until the disk filled.
        assert_eq!(ignored.matches("/.slc/records.json").count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `refresh` has a default body (a no-op) in the trait, so a delegation that
    /// forgets to forward it turns the multi-process sync into a silent no-op:
    /// the engine's periodic `SLC_VAULT_REFRESH_SECS` loop and `SlcEngine::refresh`
    /// both go through `Arc<dyn StorageBackend>`. Two stores on one vault are
    /// exactly the deployment this exists for (the MCP service and the web UI
    /// share the vault; files are the source of truth).
    #[tokio::test]
    async fn multi_process_refresh_survives_the_dyn_delegation_the_engine_uses() {
        let root = tmp_vault("refresh-dyn-delegation");
        let store = std::sync::Arc::new(ObsidianVaultStore::open(&root, false).unwrap());
        // The exact handle shape `SlcEngine` stores.
        let dynamic: std::sync::Arc<dyn StorageBackend> = store.clone();

        // Another service sharing the vault writes a note.
        let other = ObsidianVaultStore::open(&root, false).unwrap();
        other.kb_insert(&doc_for("refresh-foreign")).await.unwrap();
        assert!(
            dynamic.kb_get("refresh-foreign").await.unwrap().is_none(),
            "the stale index must not see the foreign write before the resync"
        );

        dynamic.refresh().await.unwrap();

        assert!(
            dynamic.kb_get("refresh-foreign").await.unwrap().is_some(),
            "refresh must re-read the shared vault through Arc<dyn StorageBackend>"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    /// The storage-level rename must stay the atomic index/file move: the
    /// default body is `kb_get` + `kb_insert` + `kb_purge`, which rewrites the
    /// note, bumps `version`/`updated_at` (so a rename shows up as an edit in
    /// every `by_updated` listing) and is not atomic.
    ///
    /// The moved note's frontmatter id is part of the same contract: the vault
    /// files are the source of truth, so a note left with `id: <old>` is
    /// re-registered under the old id (and resolved to a path that no longer
    /// exists) by the next index rebuild.
    #[tokio::test]
    async fn rename_through_the_dyn_delegation_does_not_rewrite_the_note() {
        let root = tmp_vault("rename-dyn-delegation");
        let store = std::sync::Arc::new(ObsidianVaultStore::open(&root, false).unwrap());
        let dynamic: std::sync::Arc<dyn StorageBackend> = store.clone();

        let before = doc_for("rename-source");
        dynamic.kb_insert(&before).await.unwrap();

        assert!(
            dynamic
                .kb_rename("rename-source", "rename-target")
                .await
                .unwrap()
        );

        let after = dynamic
            .kb_get("rename-target")
            .await
            .unwrap()
            .expect("the note must exist under the new id");
        assert_eq!(
            after.document_id, "rename-target",
            "the moved note must carry its new id in the frontmatter"
        );
        assert_eq!(
            after.version, before.version,
            "an atomic rename must not bump the document version"
        );
        assert_eq!(
            after.updated_at, before.updated_at,
            "an atomic rename must not make the document look freshly edited"
        );
        assert!(dynamic.kb_get("rename-source").await.unwrap().is_none());
        assert!(
            !root.join("rename-source.md").exists(),
            "the old note file must be gone after the rename"
        );

        // A rescan (restart / the engine's periodic refresh / another service)
        // must still resolve the document under its new id.
        let rescan = ObsidianVaultStore::open(&root, false).unwrap();
        assert!(
            rescan.kb_get("rename-target").await.unwrap().is_some(),
            "the rename must survive an index rebuild"
        );
        assert!(rescan.kb_get("rename-source").await.unwrap().is_none());

        std::fs::remove_dir_all(root).unwrap();
    }
}
