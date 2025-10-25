package model

import "time"

// SessionSummary holds lightweight information about a Codex session.
type SessionSummary struct {
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

// SessionMeta represents metadata stored in the session_meta payload.
type SessionMeta struct {
	ID         string
	Path       string
	CWD        string
	Originator string
	CLIVersion string
	StartedAt  time.Time
}

// Event represents a single entry in the session JSONL stream.
type Event struct {
	Timestamp   time.Time
	Kind        string
	Role        string
	MessageType string
	Content     []ContentBlock
	Raw         string
}

// ContentBlock models a portion of a response payload.
type ContentBlock struct {
	Type string
	Text string
}
