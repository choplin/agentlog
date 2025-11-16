// Package codex provides types and parsers for Codex CLI session logs.
package codex

import (
	"agentlog/internal/model"
	"time"
)

// Codex-specific types and constants

// EntryType represents the top-level "type" field values observed in Codex JSONL logs.
type EntryType string

const (
	EntryTypeSessionMeta  EntryType = "session_meta"
	EntryTypeResponseItem EntryType = "response_item"
	EntryTypeEventMsg     EntryType = "event_msg"
	EntryTypeTurnContext  EntryType = "turn_context"
)

// ResponseItemType captures the "payload.type" values in response_item entries.
type ResponseItemType string

const (
	ResponseItemTypeMessage              ResponseItemType = "message"
	ResponseItemTypeReasoning            ResponseItemType = "reasoning"
	ResponseItemTypeFunctionCall         ResponseItemType = "function_call"
	ResponseItemTypeFunctionCallOutput   ResponseItemType = "function_call_output"
	ResponseItemTypeCustomToolCall       ResponseItemType = "custom_tool_call"
	ResponseItemTypeCustomToolCallOutput ResponseItemType = "custom_tool_call_output"
)

// EventMsgType captures the "payload.type" values in event_msg entries.
type EventMsgType string

const (
	EventMsgTypeTokenCount     EventMsgType = "token_count"
	EventMsgTypeAgentReasoning EventMsgType = "agent_reasoning"
	EventMsgTypeUserMessage    EventMsgType = "user_message"
	EventMsgTypeAgentMessage   EventMsgType = "agent_message"
	EventMsgTypeTurnAborted    EventMsgType = "turn_aborted"
)

// PayloadRole captures the "payload.role" values observed in Codex response items.
type PayloadRole string

const (
	PayloadRoleUser      PayloadRole = "user"
	PayloadRoleAssistant PayloadRole = "assistant"
	PayloadRoleTool      PayloadRole = "tool"
	PayloadRoleSystem    PayloadRole = "system"
)

// CodexSessionSummary holds lightweight information about a Codex session.
type CodexSessionSummary struct {
	ID              string
	Path            string
	CWD             string
	Originator      string
	CLIVersion      string
	StartedAt       time.Time
	Summary         string
	MessageCount    int
	DurationSeconds int
}

// GetID returns the session ID.
func (s *CodexSessionSummary) GetID() string { return s.ID }

// GetPath returns the file path.
func (s *CodexSessionSummary) GetPath() string { return s.Path }

// GetCWD returns the working directory.
func (s *CodexSessionSummary) GetCWD() string { return s.CWD }

// GetStartedAt returns the start timestamp.
func (s *CodexSessionSummary) GetStartedAt() time.Time { return s.StartedAt }

// GetSummary returns the summary text.
func (s *CodexSessionSummary) GetSummary() string { return s.Summary }

// GetMessageCount returns the number of messages.
func (s *CodexSessionSummary) GetMessageCount() int { return s.MessageCount }

// GetDurationSeconds returns the session duration in seconds.
func (s *CodexSessionSummary) GetDurationSeconds() int { return s.DurationSeconds }

// CodexSessionMeta represents metadata stored in the session_meta payload.
type CodexSessionMeta struct {
	ID         string
	Path       string
	CWD        string
	Originator string
	CLIVersion string
	StartedAt  time.Time
}

// GetID returns the session ID.
func (m *CodexSessionMeta) GetID() string { return m.ID }

// GetPath returns the file path.
func (m *CodexSessionMeta) GetPath() string { return m.Path }

// GetCWD returns the working directory.
func (m *CodexSessionMeta) GetCWD() string { return m.CWD }

// GetStartedAt returns the start timestamp.
func (m *CodexSessionMeta) GetStartedAt() time.Time { return m.StartedAt }

// CodexEvent represents a single entry in the Codex session JSONL stream.
type CodexEvent struct {
	Timestamp   time.Time
	Kind        EntryType
	Role        PayloadRole
	PayloadType string // response_item: ResponseItemType, event_msg: EventMsgType
	Content     []model.ContentBlock
	Raw         string
}

// GetTimestamp returns the event timestamp.
func (e *CodexEvent) GetTimestamp() time.Time { return e.Timestamp }

// GetContent returns the content blocks.
func (e *CodexEvent) GetContent() []model.ContentBlock { return e.Content }

// GetRaw returns the raw JSON string.
func (e *CodexEvent) GetRaw() string { return e.Raw }

// GetRole returns a normalized role string for the event.
// For Codex events, we use the PayloadRole if available, otherwise Kind.
func (e *CodexEvent) GetRole() string {
	if e.Role != "" {
		return string(e.Role)
	}
	return string(e.Kind)
}

// IsConversation returns true if this event is part of the user/assistant conversation.
// Returns false for system messages, metadata, and internal events.
func (e *CodexEvent) IsConversation() bool {
	// Conversation: user and assistant response_item entries
	return e.Kind == EntryTypeResponseItem && (e.Role == PayloadRoleUser || e.Role == PayloadRoleAssistant)
}
