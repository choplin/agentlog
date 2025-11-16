# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Support for Claude Code session logs
- `--agent` flag to specify agent type (`codex` or `claude`)
- `AGENTLOG_AGENT` environment variable for default agent selection
- `--level` flag for verbosity control (`conversation` or `all`)
- Slash command detection and formatting in output

### Changed

- **BREAKING**: Renamed project from `codexlog` to `agentlog`
- **BREAKING**: Renamed environment variable from `CODEXLOG_SESSIONS_DIR` to `AGENTLOG_SESSIONS_DIR`
- **BREAKING**: Renamed binary from `codexlog` to `agentlog`
- **BREAKING**: Changed Homebrew tap from `choplin/tap/codexlog` to `choplin/tap/agentlog`
- Default agent type is now Claude Code (`claude`)
- Default sessions directory is now agent-specific (`~/.claude/projects` or `~/.codex/sessions`)
- Internal architecture refactored to use agent-agnostic interfaces
- Updated project description to reflect support for AI agent conversation logs in general

### Removed

- Codex-specific filter options (`--entry-type`, `--response-type`, `--event-msg-type`, `--payload-role`)
- Complex filtering logic that was tightly coupled to Codex CLI internals

## [0.1.0] - 2025-11-06

### Added

- Initial release of agentlog
- `list` command: Browse Codex session history with multiple output formats (table, plain, JSON, JSONL)
- `info` command: Display detailed session metadata with text and JSON formats
- `view` command: Render session transcripts in multiple formats (text, chat, raw)
- Chat format with role-based bubble alignment and color coding
- Filter support for entry types, roles, and response types
- Session duration tracking and display
- Support for all Codex log entry types (response_item, event_msg, turn_context, session_meta)
- Proper handling of encrypted reasoning with summary fallback
- Function call and function output display with JSON formatting
- Response item type labels for better clarity
- Environment variable support (AGENTLOG_SESSIONS_DIR)

[0.1.0]: https://github.com/choplin/agentlog/releases/tag/v0.1.0
