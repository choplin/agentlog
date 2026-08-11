//! Local catalog and read-only provider synchronization for Agentlog.
//!
//! This package owns Agentlog's configuration and `SQLite` catalog. It reads
//! Codex, Claude Code, `OpenCode`, Gemini CLI, Cursor, and Kimi Code histories
//! without writing to provider-owned paths.

pub mod app;
pub mod cli;
pub mod display;
pub mod paths;
pub mod providers;
pub mod storage;
pub mod tui;

// Keep provider modules public for callers that need provider-specific source
// discovery while shared workflows use the unified provider boundary.
pub use providers::{claude, codex, cursor, gemini, kimi, opencode};
