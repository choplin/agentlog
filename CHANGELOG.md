# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2025-11-06

### Added

- Initial release of codexlog
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
- Environment variable support (CODEXLOG_SESSIONS_DIR)

[0.1.0]: https://github.com/choplin/codexlog/releases/tag/v0.1.0
