// Package claude provides types and parsers for Claude Code session logs.
package claude

import (
	"agentlog/internal/model"
	"time"
)

// Claude Code-specific types and constants

// EntryType represents the top-level "type" field values in Claude Code JSONL logs.
type EntryType string

const (
	EntryTypeUser                EntryType = "user"
	EntryTypeAssistant           EntryType = "assistant"
	EntryTypeSummary             EntryType = "summary"
	EntryTypeFileHistorySnapshot EntryType = "file-history-snapshot"
	EntryTypeSystem              EntryType = "system"
)

// ContentBlockType represents the "type" field in content blocks.
type ContentBlockType string

const (
	ContentBlockTypeText       ContentBlockType = "text"
	ContentBlockTypeToolUse    ContentBlockType = "tool_use"
	ContentBlockTypeToolResult ContentBlockType = "tool_result"
)

// ClaudeSessionSummary represents a Claude Code session summary for listing.
type ClaudeSessionSummary struct {
	ID              string    // Session ID (typically the filename without extension)
	Path            string    // Full path to the JSONL file
	CWD             string    // Working directory
	Version         string    // Claude Code version
	StartedAt       time.Time // First message timestamp
	Summary         string    // First user message or summary text
	MessageCount    int       // Number of messages (user + assistant)
	DurationSeconds int       // Session duration in seconds
}

// GetID returns the session ID.
func (s *ClaudeSessionSummary) GetID() string { return s.ID }

// GetPath returns the file path.
func (s *ClaudeSessionSummary) GetPath() string { return s.Path }

// GetCWD returns the working directory.
func (s *ClaudeSessionSummary) GetCWD() string { return s.CWD }

// GetStartedAt returns the start timestamp.
func (s *ClaudeSessionSummary) GetStartedAt() time.Time { return s.StartedAt }

// GetSummary returns the summary text.
func (s *ClaudeSessionSummary) GetSummary() string { return s.Summary }

// GetMessageCount returns the number of messages.
func (s *ClaudeSessionSummary) GetMessageCount() int { return s.MessageCount }

// GetDurationSeconds returns the session duration in seconds.
func (s *ClaudeSessionSummary) GetDurationSeconds() int { return s.DurationSeconds }

// ClaudeSessionMeta represents metadata from a Claude Code session.
type ClaudeSessionMeta struct {
	ID        string    // Session ID
	Path      string    // File path
	CWD       string    // Working directory
	Version   string    // Claude Code version
	StartedAt time.Time // First message timestamp
}

// GetID returns the session ID.
func (m *ClaudeSessionMeta) GetID() string { return m.ID }

// GetPath returns the file path.
func (m *ClaudeSessionMeta) GetPath() string { return m.Path }

// GetCWD returns the working directory.
func (m *ClaudeSessionMeta) GetCWD() string { return m.CWD }

// GetStartedAt returns the start timestamp.
func (m *ClaudeSessionMeta) GetStartedAt() time.Time { return m.StartedAt }

// ClaudeEvent represents a single entry in the Claude Code session JSONL stream.
type ClaudeEvent struct {
	Timestamp time.Time
	Kind      EntryType
	Role      string // "user", "assistant"
	Content   []model.ContentBlock
	Raw       string

	// Metadata fields
	UUID       string
	ParentUUID string
	SessionID  string
	CWD        string
	Version    string

	// Assistant-specific fields
	MessageID string
	RequestID string
	Model     string
	Usage     *TokenUsage

	// Summary-specific fields
	SummaryText string
	LeafUUID    string
}

// TokenUsage represents token usage statistics for assistant messages.
type TokenUsage struct {
	InputTokens              int
	CacheCreationInputTokens int
	CacheReadInputTokens     int
	OutputTokens             int
	ServiceTier              string
}

// GetTimestamp returns the event timestamp.
func (e *ClaudeEvent) GetTimestamp() time.Time { return e.Timestamp }

// GetContent returns the content blocks.
func (e *ClaudeEvent) GetContent() []model.ContentBlock { return e.Content }

// GetRaw returns the raw JSON string.
func (e *ClaudeEvent) GetRaw() string { return e.Raw }

// GetRole returns the role string for the event.
func (e *ClaudeEvent) GetRole() string {
	if e.Role != "" {
		return e.Role
	}
	return string(e.Kind)
}
