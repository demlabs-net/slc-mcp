//! Web frontend: REST API + SPA static files, embedded right into the MCP
//! server.
//!
//! A single process owns the vault — no concurrent-access conflicts (the
//! refresh loop remains for external vault changes, e.g. manual edits in
//! Obsidian or `git pull`). REST handlers go through the same
//! `Arc<SlcEngine>` as the MCP tools.

pub mod api;
pub mod static_files;
