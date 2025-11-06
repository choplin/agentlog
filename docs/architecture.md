---
created: 2025-11-06
updated: 2025-11-06
---

# Architecture

This document describes the architecture of codexlog, a CLI tool for browsing and analyzing Codex session logs.

## Overview

codexlog is a Go-based CLI application that parses JSONL (JSON Lines) session logs produced by the Codex CLI and provides various viewing and filtering capabilities.

```
┌─────────────┐
│   CLI       │  Cobra-based command interface
│  (cmd/)     │  - list, info, view commands
└──────┬──────┘
       │
       ├─────────────────────────────────────┐
       │                                     │
┌──────▼───────┐  ┌──────────────┐  ┌───────▼────────┐
│   Store      │  │   Parser     │  │    Format      │
│ (internal/)  │  │ (internal/)  │  │  (internal/)   │
│              │  │              │  │                │
│ - Session    │  │ - JSONL      │  │ - Table output │
│   discovery  │  │   parsing    │  │ - Plain text   │
│ - Filtering  │  │ - Event      │  │ - JSON/JSONL   │
│              │  │   decoding   │  │                │
└──────────────┘  └──────────────┘  └────────────────┘
                          │
                  ┌───────▼────────┐
                  │     Model      │
                  │  (internal/)   │
                  │                │
                  │ - Event        │
                  │ - SessionMeta  │
                  │ - ContentBlock │
                  └────────────────┘
```

## Package Structure

### cmd/codexlog

Entry point for the CLI application. Contains:

- `main.go`: Root command and subcommands (list, info, view)
- Command-line flag definitions
- Output formatting coordination

### internal/model

Core data structures representing Codex log entries:

- `Event`: Represents a single log entry
- `SessionMeta`: Session metadata (ID, timestamp, CWD, etc.)
- `EntryType`: Type constants (response_item, event_msg, turn_context, session_meta)
- `PayloadRole`: Role constants (user, assistant, tool, system)
- `ContentBlock`: Structured content representation

### internal/parser

JSONL parsing and event decoding:

- `ReadSessionMeta()`: Extracts session metadata from the first session_meta entry
- `FirstUserSummary()`: Gets the first user message and counts response items
- `IterateEvents()`: Streams events from a JSONL file
- `parseEvent()`: Converts raw JSON to Event structs

**Key responsibilities:**

- Handle different payload structures (function_call, event_msg, turn_context, etc.)
- Extract content from nested JSON fields
- Support encrypted reasoning with summary fallback
- Parse function call outputs from the `output` field

### internal/store

Session discovery and management:

- `ListSessions()`: Finds and lists sessions with metadata
- `FindSessionPath()`: Resolves session IDs to file paths
- Filtering by CWD, date range, and limits
- Duration calculation from session start to last event

### internal/format

Output formatting for list and info commands:

- `WriteSummaries()`: Formats session lists (table, plain, JSON, JSONL)
- `RenderEvent()`: Formats individual events for display
- Table formatting with go-pretty
- JSON indentation and wrapping

### internal/view

Advanced transcript rendering:

- `Run()`: Main entry point for view command
- Event filtering by type, role, and response type
- Chat format with role-based bubble alignment
- Color coding for different roles
- Text wrapping and width management

## Data Flow

### List Command

```
User → CLI (list) → Store.ListSessions()
                     ↓
                   Scan directory tree
                     ↓
                   Parser.ReadSessionMeta() for each file
                     ↓
                   Parser.FirstUserSummary() for each file
                     ↓
                   Filter & sort sessions
                     ↓
                   Format.WriteSummaries()
                     ↓
                   Output (table/plain/JSON/JSONL)
```

### Info Command

```
User → CLI (info) → Store.FindSessionPath()
                     ↓
                   Parser.ReadSessionMeta()
                     ↓
                   Parser.FirstUserSummary()
                     ↓
                   Calculate duration
                     ↓
                   Format as text or JSON
                     ↓
                   Output
```

### View Command

```
User → CLI (view) → Store.FindSessionPath()
                     ↓
                   Parser.IterateEvents()
                     ↓
                   Filter events (by type/role/response type)
                     ↓
                   Limit to --max events
                     ↓
                   View.Run() → Render as text or chat
                     ↓
                   Output
```

## Key Design Decisions

### Event Parsing

- **Specialized payload structures**: Different structs for each entry type (functionCallPayload, eventMsgPayload, turnContextPayload) to handle varying JSON structures
- **Fallback logic**: When expected fields are missing (e.g., turn_context without turn_id), fall back to alternative fields
- **Content blocks**: Normalize all content into ContentBlock arrays for consistent rendering

### Chat View

- **Role-based alignment**: User messages right-aligned, assistant left-aligned, system/tool centered
- **Color coding**: Each role has a distinct color (extracted from raw role, not display label)
- **Bubble rendering**: Box-drawing characters with padding and wrapping

### Session Discovery

- **Hierarchical scanning**: Sessions are stored in `YYYY/MM/DD/` subdirectories
- **ID prefix matching**: Support partial session IDs for convenience
- **Lazy loading**: Only parse files when needed to improve performance

### Filtering

- **Composable filters**: Multiple filter dimensions (entry type, role, response type, event_msg type)
- **Default filters**: View command defaults to user+assistant messages only
- **--all flag**: Override all filters to show everything

## Extension Points

### Adding New Commands

1. Create command function in `cmd/codexlog/main.go`
2. Add to `rootCmd.AddCommand()`
3. Use existing internal packages for implementation

### Supporting New Entry Types

1. Add constants to `internal/model/event.go`
2. Add payload struct to `internal/parser/parser.go`
3. Add case in `parseEvent()` switch statement
4. Update format/view packages as needed

### Adding New Output Formats

1. Add format string to command flags
2. Implement formatting logic in `internal/format/` or `internal/view/`
3. Update format switch in command handler

## Testing Strategy

- **Unit tests**: Each internal package has `*_test.go` files
- **Test data**: `testdata/sessions/` contains sample JSONL files
- **Integration tests**: Command-level tests in `cmd/codexlog/`
- **CI**: GitHub Actions runs tests on every push

## Dependencies

- `github.com/spf13/cobra`: CLI framework
- `github.com/jedib0t/go-pretty/v6`: Table formatting
- `github.com/mattn/go-isatty`: Terminal detection
- `github.com/mattn/go-runewidth`: Unicode width calculation
- `golang.org/x/term`: Terminal size detection

## Build & Release

See [development.md](development.md) for complete build, test, and release instructions including:

- Building and version injection
- Running tests and linters
- Release process with GoReleaser
- CI/CD workflows
