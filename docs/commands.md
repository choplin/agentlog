---
created: 2025-11-06
updated: 2025-11-06
---

# Command Reference

codexlog provides three main commands: `list`, `info`, and `view`.

## Overview

```
codexlog - Browse, search, and analyze Codex CLI sessions

Usage:
  codexlog [command]

Available Commands:
  list        List session metadata in reverse chronological order
  info        Show session metadata and file details
  view        Render a session transcript
  help        Help about any command
  version     Show version information

Flags:
  -h, --help      help for codexlog
  -v, --version   version for codexlog
```

## Global Flags

### --sessions-dir

Available for all commands. Specifies the path to the sessions directory.

```bash
codexlog list --sessions-dir /custom/path/to/sessions
```

**Default value**:

1. Value of the `CODEXLOG_SESSIONS_DIR` environment variable if set
2. Otherwise `~/.codex/sessions`

**Example environment variable setup**:

```bash
export CODEXLOG_SESSIONS_DIR=/custom/sessions/path
```

## list command

Displays a list of sessions in reverse chronological order (newest first).

### Usage

```bash
codexlog list [flags]
```

### Flags

#### --cwd <path>

Filter sessions with a cwd that matches the specified path.

```bash
codexlog list --cwd /Users/alice/my-project
```

#### --all

Include sessions from all directories. By default, only sessions matching the current working directory are shown.

```bash
codexlog list --all
```

**Note**: `--all` and `--cwd` cannot be used simultaneously.

#### --after <timestamp>

Include sessions started after the specified RFC3339 timestamp.

```bash
codexlog list --after 2025-01-15T00:00:00Z
```

#### --before <timestamp>

Include sessions started before the specified RFC3339 timestamp.

```bash
codexlog list --before 2025-01-20T23:59:59Z
```

#### --limit <n>

Limit the number of sessions returned (0 = no limit).

```bash
codexlog list --limit 10
```

#### --format <format>

Specify output format: `table`, `plain`, `json`, or `jsonl`.

```bash
codexlog list --format json
```

**Default**: `table`

#### --no-header

Omit the header row in plain output.

```bash
codexlog list --format plain --no-header
```

#### --summary-width <n>

Specify the maximum number of characters to include in the summary column.

```bash
codexlog list --summary-width 200
```

**Default**: 160

### Output Formats

#### table (default)

Displays in a human-readable table format.

```
╭──────────────┬──────────────────────┬──────────┬──────┬──────────────────────────────────╮
│ SESSION ID   │ STARTED AT           │ DURATION │ MSGS │ SUMMARY                          │
├──────────────┼──────────────────────┼──────────┼──────┼──────────────────────────────────┤
│ 0193a4b2...  │ 2025-01-15T10:30:00Z │ 00:15:42 │   25 │ Write a fibonacci function       │
│ 0193a4b1...  │ 2025-01-15T09:15:30Z │ 00:08:15 │   12 │ Fix the authentication bug       │
╰──────────────┴──────────────────────┴──────────┴──────┴──────────────────────────────────╯
```

#### plain

Tab-delimited format suitable for processing in scripts.

```
SESSION_ID	STARTED_AT	DURATION	MSGS	SUMMARY
0193a4b2-8c90-7d4e-a123-456789abcdef	2025-01-15T10:30:00Z	00:15:42	25	Write a fibonacci function
0193a4b1-1234-5678-9abc-def012345678	2025-01-15T09:15:30Z	00:08:15	12	Fix the authentication bug
```

Using `--no-header` omits the header row.

#### json

Outputs all sessions as a single JSON array.

```json
[
  {
    "id": "0193a4b2-8c90-7d4e-a123-456789abcdef",
    "path": "/Users/alice/.codex/sessions/2025/01/15/0193a4b2-8c90-7d4e-a123-456789abcdef.jsonl",
    "cwd": "/Users/alice/project",
    "originator": "cli",
    "cli_version": "1.2.0",
    "started_at": "2025-01-15T10:30:00Z",
    "summary": "Write a fibonacci function",
    "message_count": 25,
    "duration_seconds": 942
  }
]
```

#### jsonl

Outputs each session as one line of JSON (JSON Lines format).

```jsonl
{"id":"0193a4b2-8c90-7d4e-a123-456789abcdef","path":"...","cwd":"...","message_count":25,"duration_seconds":942}
{"id":"0193a4b1-1234-5678-9abc-def012345678","path":"...","cwd":"...","message_count":12,"duration_seconds":495}
```

### Usage Examples

```bash
# Display sessions for the current directory
codexlog list

# Display all sessions
codexlog list --all

# Display sessions for a specific project
codexlog list --cwd /Users/alice/my-project

# Display the 10 most recent sessions
codexlog list --limit 10

# Display sessions within a specific date range
codexlog list --after 2025-01-01T00:00:00Z --before 2025-01-31T23:59:59Z

# Output as JSON and process with jq
codexlog list --all --format json | jq '.[] | select(.message_count > 20)'
```

## info command

Displays detailed metadata for a session.

### Usage

```bash
codexlog info <session-id-or-path> [flags]
```

### Arguments

- `session-id-or-path`: Session ID (full or prefix), or path to a JSONL file

### Session ID Resolution

The command resolves sessions in the following order:

1. If the argument is an existing file path, use it
2. Attempt to resolve as a relative path within `sessions-dir`
3. Attempt prefix matching for session ID

**Examples**:

```bash
# Full session ID
codexlog info 0193a4b2-8c90-7d4e-a123-456789abcdef

# Session ID prefix
codexlog info 0193a4b2

# Absolute path
codexlog info /Users/alice/.codex/sessions/2025/01/15/0193a4b2-8c90-7d4e-a123-456789abcdef.jsonl

# Relative path (from sessions-dir)
codexlog info 2025/01/15/0193a4b2-8c90-7d4e-a123-456789abcdef.jsonl
```

### Flags

#### --format <format>

Specify output format: `text` or `json`.

```bash
codexlog info 0193a4b2 --format json
```

**Default**: `text`

#### --summary <mode>

Specify how to display the summary: `clip` or `full`.

```bash
codexlog info 0193a4b2 --summary full
```

**Default**: `clip` (truncated at 160 characters)

### Output Formats

#### text (default)

Displays in a human-readable format.

```
Session ID    : 0193a4b2-8c90-7d4e-a123-456789abcdef
Started At    : 2025-01-15T10:30:00Z
Duration      : 00:15:42
CWD           : /Users/alice/project
Originator    : cli
CLI Version   : 1.2.0
Message Count : 25
JSONL Path    : /Users/alice/.codex/sessions/2025/01/15/0193a4b2-8c90-7d4e-a123-456789abcdef.jsonl
Summary       : Write a fibonacci function that handles edge cases properly…
```

#### json

Displays in machine-readable JSON format.

```json
{
  "session_id": "0193a4b2-8c90-7d4e-a123-456789abcdef",
  "jsonl_path": "/Users/alice/.codex/sessions/2025/01/15/0193a4b2-8c90-7d4e-a123-456789abcdef.jsonl",
  "started_at": "2025-01-15T10:30:00Z",
  "cwd": "/Users/alice/project",
  "originator": "cli",
  "cli_version": "1.2.0",
  "message_count": 25,
  "duration_seconds": 942,
  "duration_display": "00:15:42",
  "summary": "Write a fibonacci function that handles edge cases properly"
}
```

### Usage Examples

```bash
# Display in text format
codexlog info 0193a4b2

# Display in JSON format
codexlog info 0193a4b2 --format json

# Display full summary
codexlog info 0193a4b2 --summary full

# Specify path directly
codexlog info /path/to/session.jsonl
```

## view command

Renders a session transcript.

### Usage

```bash
codexlog view <session-id-or-path> [flags]
```

### Arguments

- `session-id-or-path`: Session ID (full or prefix), or path to a JSONL file

Session ID resolution is the same as the `info` command.

### Flags

#### --format <format>

Specify output format: `text`, `chat`, or `raw`.

```bash
codexlog view 0193a4b2 --format chat
```

**Default**: `text`

#### --wrap <width>

Wrap message bodies at the specified column width.

```bash
codexlog view 0193a4b2 --format chat --wrap 80
```

**Default**: 0 (no wrapping, use terminal width)

#### --max <n>

Display only the most recent N events (0 = no limit).

```bash
codexlog view 0193a4b2 --max 20
```

**Default**: 0 (display all)

#### --all

Display all entries (disable filters).

```bash
codexlog view 0193a4b2 --all --format chat
```

**Default**: false (display only user and assistant messages)

#### --entry-type / -E <types>

Specify entry types to include, comma-separated.

```bash
codexlog view 0193a4b2 -E response_item,event_msg
```

**Valid values**: `response_item`, `event_msg`, `turn_context`, `session_meta`

**Default**: `response_item`

#### --response-type / -T <types>

Specify response_item payload types to include, comma-separated.

```bash
codexlog view 0193a4b2 -T message,function_call
```

**Valid values**: `message`, `reasoning`, `function_call`, `function_call_output`, `custom_tool_call`, `custom_tool_call_output`

**Default**: `message`

#### --event-msg-type / -M <types>

Specify event_msg payload types to include, comma-separated.

```bash
codexlog view 0193a4b2 -M token_count,agent_reasoning
```

**Valid values**: `token_count`, `agent_reasoning`, `user_message`, `agent_message`, `turn_aborted`

**Default**: none (event_msg are excluded)

#### --payload-role / -R <roles>

Specify payload roles to include, comma-separated.

```bash
codexlog view 0193a4b2 -R user,assistant,tool
```

**Valid values**: `user`, `assistant`, `tool`, `system`, `all`

**Default**: `user,assistant`

#### --raw

Output raw JSONL without formatting.

```bash
codexlog view 0193a4b2 --raw
```

Outputs raw JSONL after filters are applied. Useful for debugging.

#### --color

Force enable ANSI colors even when stdout is not a TTY.

```bash
codexlog view 0193a4b2 --format chat --color | less -R
```

#### --no-color

Disable ANSI colors regardless of terminal detection.

```bash
codexlog view 0193a4b2 --format chat --no-color
```

### Output Formats

#### text (default)

Displays in numbered message format.

```
[#01][user][2025-01-15T10:30:15Z]
  Write a fibonacci function

[#02][assistant][2025-01-15T10:30:20Z]
  I'll write a fibonacci function for you.

[#03][assistant: function_call][2025-01-15T10:30:25Z]
  Function: write_file
  Arguments:
  {
    "path": "fib.py",
    "content": "def fib(n):\n  if n <= 1:\n    return n\n  return fib(n-1) + fib(n-2)"
  }

[#04][tool: function_call_output][2025-01-15T10:30:30Z]
  Output: File written successfully
```

#### chat

Displays in chat-style bubble format.

```
                                                    2025-01-15 10:30:15
                                                     ┌─────────────────┐
                                                     │ Write a         │
                                                     │ fibonacci       │
                                                     │ function        │
                                                     └─────────────────┘
                                                                    user


2025-01-15 10:30:20
┌─────────────────────────────┐
│ I'll write a fibonacci      │
│ function for you.           │
└─────────────────────────────┘
assistant


2025-01-15 10:30:25
┌─────────────────────────────┐
│ Function: write_file        │
│                             │
│ Arguments:                  │
│ {                           │
│   "path": "fib.py",         │
│   "content": "def fib..."   │
│ }                           │
└─────────────────────────────┘
assistant: function_call
```

**Features**:

- User messages are right-aligned
- Assistant messages are left-aligned
- Tool/System messages are center-aligned
- Color-coded by role
- Timestamp display

#### raw

Outputs filtered raw JSONL.

```jsonl
{"timestamp":"2025-01-15T10:30:15.123Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"Write a fibonacci function"}]}}
{"timestamp":"2025-01-15T10:30:20.456Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"I'll write a fibonacci function for you."}]}}
```

### Combining Filters

Flags can be combined:

```bash
# Display only user messages and function calls
codexlog view 0193a4b2 -R user,assistant -T message,function_call

# Display all event_msg including token counts
codexlog view 0193a4b2 -E event_msg -M token_count,agent_reasoning

# Display all tool outputs
codexlog view 0193a4b2 -R tool -T function_call_output
```

**Note**: Specifying `--all` disables all other filter flags.

### Usage Examples

```bash
# Display in text format
codexlog view 0193a4b2

# Display in chat format
codexlog view 0193a4b2 --format chat

# Display only the 20 most recent messages
codexlog view 0193a4b2 --format chat --max 20

# Wrap at 80 columns
codexlog view 0193a4b2 --format chat --wrap 80

# Display all entry types (reasoning, function_call, etc.)
codexlog view 0193a4b2 --format chat --all

# Display only user messages
codexlog view 0193a4b2 -R user

# Display function calls and outputs
codexlog view 0193a4b2 -T function_call,function_call_output

# Display token count events
codexlog view 0193a4b2 -E event_msg -M token_count

# Pipe raw JSONL to jq
codexlog view 0193a4b2 --raw | jq '.payload.type'

# Display with colors in a pager
codexlog view 0193a4b2 --format chat --color | less -R
```

## Exit Codes

codexlog uses the following exit codes:

| Code | Meaning                                           |
| ---- | ------------------------------------------------- |
| 0    | Success                                           |
| 1    | Error (file not found, parse failure, etc.)       |

Error messages are output to stderr.

## Environment Variables

### CODEXLOG_SESSIONS_DIR

Sets the default path for the sessions directory.

```bash
export CODEXLOG_SESSIONS_DIR=/custom/sessions/path
codexlog list  # Uses /custom/sessions/path
```

This environment variable can be overridden by the `--sessions-dir` flag.

## Tips

### Pipeline Processing

codexlog is suitable for use in pipelines:

```bash
# Count number of sessions
codexlog list --all --format plain --no-header | wc -l

# Find long sessions
codexlog list --all --format jsonl | jq 'select(.duration_seconds > 600)'

# Extract session IDs for a specific project
codexlog list --cwd /my/project --format plain --no-header | cut -f1
```

### Debugging

When debugging parsing issues:

```bash
# Inspect filtered raw JSONL
codexlog view 0193a4b2 --raw | jq

# Inspect only specific entry types
codexlog view 0193a4b2 -E event_msg --raw | jq
```

### Scripting

Use `--format jsonl` or `--format json` in scripts:

```bash
#!/bin/bash
# Aggregate message count for each session
codexlog list --all --format jsonl | while IFS= read -r line; do
  id=$(echo "$line" | jq -r '.id')
  count=$(echo "$line" | jq -r '.message_count')
  echo "$id: $count messages"
done
```
