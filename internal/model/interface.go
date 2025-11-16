// Package model provides common interfaces and types for agent log implementations.
package model

import "time"

// SessionSummaryProvider provides common session summary information.
// Different agent implementations (Codex, Claude) can provide agent-specific
// fields while sharing this common interface.
//
// This is used by ListSessions which scans entire session files to compute
// summary information (message count, duration, first user message).
type SessionSummaryProvider interface {
	GetID() string
	GetPath() string
	GetCWD() string
	GetStartedAt() time.Time
	GetSummary() string
	GetMessageCount() int
	GetDurationSeconds() int
}

// SessionMetaProvider provides common session metadata.
// Different agent implementations can extend this with agent-specific metadata.
//
// This is used by ReadSessionMeta which only reads the first metadata record
// from a session file (fast operation). The overlap with SessionSummaryProvider
// is intentional for performance optimization: info command uses ReadSessionMeta
// to get basic metadata quickly, then iterates events separately to count messages.
type SessionMetaProvider interface {
	GetID() string
	GetPath() string
	GetCWD() string
	GetStartedAt() time.Time
}

// EventProvider provides common event information.
// Different agent implementations can have different internal structures
// while exposing these common fields for display and filtering.
type EventProvider interface {
	GetTimestamp() time.Time
	GetRole() string // Normalized role: "user", "assistant", "tool", "system"
	GetContent() []ContentBlock
	GetRaw() string       // Raw JSON for debugging/export
	IsConversation() bool // True for user/assistant messages, false for system/metadata
}
