# codexlog

[![CI](https://github.com/choplin/codexlog/actions/workflows/ci.yml/badge.svg)](https://github.com/choplin/codexlog/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Go Version](https://img.shields.io/badge/Go-1.23+-blue.svg)](https://golang.org)

Browse, search, and analyze Codex CLI sessions.

## Why codexlog?

Ever wished you could easily search through your past Codex conversations? Or review what decisions were made in previous sessions? codexlog gives you powerful tools to explore your Codex history:

- 📊 **Instant overview** of all your sessions with duration and message counts
- 🔍 **Quick search** through conversation summaries
- 💬 **Beautiful chat view** that recreates the conversation flow
- 🎯 **Filter and analyze** specific parts of sessions

## Quick Start

```bash
# Install via Homebrew
brew install choplin/tap/codexlog

# List your recent sessions
codexlog list

# View a session in chat format
codexlog view <session-id> --format chat
```

## Features

### 📋 Browse All Sessions

![List command](assets/list.png)

See all your Codex sessions at a glance. Sort by date, filter by project directory, and quickly identify sessions by their opening message.

### 📝 Session Details

![Info command](assets/info.png)

Get the full picture: when a session started, how long it lasted, how many messages were exchanged, and where to find the raw log file.

### 📖 Read Transcripts

![View command](assets/view.png)

Review conversations in a clean, numbered format. Perfect for extracting specific information or sharing with teammates.

### 💬 Chat View

![Chat format](assets/chat.png)

Relive your conversations with a familiar chat interface. User messages on the right, assistant responses on the left, complete with timestamps and color coding.

## Installation

### Homebrew

```bash
brew install choplin/tap/codexlog
```

### From Source

```bash
go install github.com/choplin/codexlog/cmd/codexlog@latest
```

## Configuration

By default, codexlog looks for session logs in `~/.codex/sessions`. You can override this with the `CODEXLOG_SESSIONS_DIR` environment variable:

```bash
export CODEXLOG_SESSIONS_DIR=/path/to/your/sessions
```

Or use the `--sessions-dir` flag for individual commands.

## Usage

### List Sessions

```bash
# List recent sessions
codexlog list

# List all sessions
codexlog list --all

# Different output formats
codexlog list --format jsonl
codexlog list --format plain
```

### View Session Details

```bash
# Show session metadata
codexlog info <session-id>

# JSON output
codexlog info <session-id> --format json
```

### View Transcripts

```bash
# Text format with numbered messages
codexlog view <session-id>

# Chat format with bubbles
codexlog view <session-id> --format chat

# Show all entry types (including reasoning, function calls)
codexlog view <session-id> --format chat --all

# Limit to recent messages
codexlog view <session-id> --format chat --max 20
```

## Advanced Features

- **Multiple output formats**: table, plain, json, jsonl for different use cases
- **Filtering**: Filter by entry type, role, or response type
- **Session duration tracking**: See how long each session lasted
- **Full log support**: Handles all Codex entry types including encrypted reasoning
- **Color coding**: Role-based colors and bubble alignment in chat view

## License

MIT
