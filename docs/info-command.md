---
created: 2025-10-25
updated: 2025-10-25
---

# `codexlog info` Output Plan

## Goal

Improve the `codexlog info` subcommand so that contributors can quickly inspect key session metadata in both JSON and human-readable formats without running additional utilities.

## Scope

- Support richer text output that mirrors the JSON payload while adding friendly formatting.
- Ensure duration data, message counts, and source file paths are surfaced consistently.
- Preserve the existing machine-consumable JSON format for scripts and tools.

## Target Behaviors

- Display summary fields in this order for both formats: Session ID, Started At, Duration, CWD, CLI Version, Message Count, JSONL Path, Summary snippet.
- Format durations as `HH:MM:SS`, falling back to `00:00:00` when the end timestamp is missing.
- Truncate summaries at 160 characters for text mode and append an ellipsis when truncated.
- Highlight absolute paths and timestamps to avoid ambiguity when multiple sessions share similar IDs.

## CLI Usage

- `codexlog info <session-id>`: resolves the session either by ID or relative path.
- `--format text` (default) presents a human-readable block with labeled fields.
- `--format json` emits the same fields as structured JSON with camelCase keys.
- `--sessions-dir` keeps pointing to `~/.codex/sessions` but remains overrideable for tests.
- `--summary clip|full` controls whether the summary is trimmed (clip) or shown in full.

## Acceptance Checklist

- [x] Text mode includes every field enumerated above with clear labels.
- [x] JSON mode retains backwards compatibility while adding `durationSeconds` and `durationDisplay`.
- [x] Summary presentation can be toggled via the `--summary` flag.
- [x] Error messages for missing sessions remain unchanged.
- [x] Unit tests cover both formats and validate duration formatting.
