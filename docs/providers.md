# Provider reference

Agentlog supports Codex, Claude Code, OpenCode, Gemini CLI, Cursor, and Kimi
Code. All collectors emit the catalog model defined in
[Data architecture](data-architecture.md#stored-catalog-data).

## Common contract

| Rule | Behavior |
| --- | --- |
| Ownership | Provider sources are read only; normalized results go only to Agentlog's catalog. |
| Override | An absolute `providers.*_root` in `config.toml` has highest precedence. Relative roots are rejected. |
| Agentlog home | `--home` and `AGENTLOG_HOME` never redirect provider discovery. |
| Discovery error | Fails only that provider and does not mark its retained sources missing. |
| Source error | Records `error`, preserves last-good sessions, and continues with other sources. |
| Missing source | After completed discovery, records `missing` and preserves last-good sessions. |
| Empty conversation | A valid source can refresh successfully without creating a session. |

## Source discovery and root precedence

| Provider | Root precedence | Source | Session identity |
| --- | --- | --- | --- |
| Codex | config → `CODEX_HOME` → `$HOME/.codex` | `sessions/**/*.jsonl`; legacy `history.jsonl` | Current `session_meta.payload.id`; legacy `session_id` |
| Claude Code | config → `CLAUDE_CONFIG_DIR` → `$HOME/.claude` | Parent/subagent JSONL below `projects/` | Transcript `sessionId` |
| OpenCode | config → absolute `XDG_DATA_HOME/opencode` → OS default | `opencode.db` and active WAL via private snapshot | SQLite `session.id` |
| Gemini CLI | config → `$GEMINI_CLI_HOME/.gemini` → `$HOME/.gemini` | JSONL recursively below `tmp/` | Native `sessionId` |
| Cursor | config → `$HOME/.cursor` | Any JSONL recursively below `projects/` | Containing directory name |
| Kimi Code | config → `KIMI_CODE_HOME` → `$HOME/.kimi-code` | `sessions/*/*/agents/*/wire.jsonl` plus index/state | Validated path, index, session, and agent identity |

OpenCode OS defaults:

| OS | Default root |
| --- | --- |
| macOS | `$HOME/Library/Application Support/opencode` |
| Linux | `$HOME/.local/share/opencode` |

File-tree discovery skips symbolic links. A missing discovery directory is an
empty source set; other enumeration errors fail provider discovery.

Configuration example:

```toml
[providers]
codex_root = "/absolute/path/to/codex/history"
claude_root = "/absolute/path/to/claude/config"
opencode_root = "/absolute/path/to/opencode/data"
gemini_root = "/absolute/path/to/gemini/config"
cursor_root = "/absolute/path/to/cursor/config"
kimi_root = "/absolute/path/to/kimi-code/config"
```

## Read-only provider boundary

| Provider | Read strategy | Concurrent-change check |
| --- | --- | --- |
| Codex | Bounded JSONL stream | No after-read fingerprint comparison |
| Claude Code | Bounded JSONL stream | Before/after source fingerprint |
| Gemini CLI | Bounded JSONL stream | Before/after source fingerprint |
| Cursor | Bounded JSONL stream | Before/after source fingerprint |
| Kimi Code | Bounded index, state, and wire reads | Before/after combined fingerprint |
| OpenCode | Copy bounded main/WAL; open only the private copy with SQLite | Re-read and compare main/WAL/SHM set |

No collector removes, rewrites, truncates, checkpoints, or archives a provider
source. Recursive file discovery does not follow symlinks.

For OpenCode, the SHM bytes participate in the size/stability check but are not
copied: SQLite creates a private SHM beside the copied database and WAL.

## Provider projections

| Provider | Projection notes |
| --- | --- |
| Codex | Current response/event items plus lower-fidelity legacy prompts; current metadata can include CWD, repository, model, execution kind, and time |
| Claude Code | Distinguishes project and `subagents` JSONL; retains visible text and tool names, not thinking or tool payloads |
| OpenCode | Validates `session`/`message`/`part` schema and identities; joins supported text/tool parts and available metadata |
| Gemini CLI | Reads supported user/Gemini content; nested JSONL files remain distinct sessions |
| Cursor | Accepts arbitrarily named project JSONL; containing directory supplies session identity |
| Kimi Code | Requires path/index/state agreement; distinguishes primary and subagent execution kinds |

## Failure isolation and last-good data

Synchronization runs providers sequentially. Each provider scan uses one catalog
transaction.

| Failure point | Result |
| --- | --- |
| Root resolution or discovery | Provider reports failure; later providers run; no missing marks |
| One source parse/stability check | Source becomes `error`; last-good retained; other sources continue |
| Valid content exceeds selection bounds | Bounded snapshot stored as `partial` |
| Known source absent after completed discovery | Source becomes `missing`; last-good retained |
| Catalog/storage write | Provider transaction rolls back |

`list`, `show`, and the TUI expose `ok`, `partial`, `error`, or `missing` with
retained sessions. `error` and `missing` identify last-good data, not a current
successful read.

## Safety bounds

| Collector | Source/file-set | Record/query | Selected catalog content |
| --- | --- | --- | --- |
| Codex | 64 MiB/source | 1 MiB/JSONL record | 128 KiB/text item; 2 MiB and 2,000 items/session; bounded legacy totals |
| Claude, Gemini, Cursor, Kimi | 64 MiB/source | 4 MiB/JSONL record | 128 KiB/text item; 2 MiB and 2,000 items/source or session projection |
| OpenCode | 128 MiB main/WAL/SHM set | 100,000 joined rows; 64 MiB joined JSON | 512-byte identities; 128 KiB/text item; 2 MiB and 2,000 items/session |

These are safety bounds, not performance targets. Malformed, unsupported, or
oversized sources fail and retain last-good data. A supported snapshot truncated
only at selection bounds is stored as `partial`.
