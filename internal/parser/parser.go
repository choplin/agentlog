package parser

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"

	"codexlog/internal/model"
)

// ErrSessionMetaNotFound is returned when a JSONL file lacks session_meta.
var ErrSessionMetaNotFound = errors.New("session_meta record not found")

// ReadSessionMeta loads metadata from the first session_meta record in path.
func ReadSessionMeta(path string) (*model.SessionMeta, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close()

	scanner := newScanner(file)
	for scanner.Scan() {
		recBytes := scanner.Bytes()
		meta, ok, err := tryParseMeta(recBytes)
		if err != nil {
			return nil, fmt.Errorf("parse session_meta: %w", err)
		}
		if ok {
			meta.Path = path
			return meta, nil
		}
	}

	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("scan session: %w", err)
	}

	return nil, ErrSessionMetaNotFound
}

// FirstUserSummary returns the first user message text (trimmed) and total
// number of response_item entries found in the session.
func FirstUserSummary(path string) (summary string, messageCount int, err error) {
	file, err := os.Open(path)
	if err != nil {
		return "", 0, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close()

	scanner := newScanner(file)
	for scanner.Scan() {
		recBytes := scanner.Bytes()
		event, err := parseEvent(recBytes)
		if err != nil {
			return "", messageCount, err
		}

		if event.Kind == "response_item" {
			messageCount++
			if summary == "" && event.Role == "user" {
				summary = buildSummaryText(event.Content)
			}
		}
	}

	if err := scanner.Err(); err != nil {
		return summary, messageCount, fmt.Errorf("scan session: %w", err)
	}

	return summary, messageCount, nil
}

// IterateEvents walks through the session JSONL file and calls fn for each
// decoded event. If roleFilter is non-empty, only events with matching role are
// forwarded (session_meta is always forwarded).
func IterateEvents(path string, roleFilter string, fn func(model.Event) error) error {
	file, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("open session file: %w", err)
	}
	defer file.Close()

	scanner := newScanner(file)
	for scanner.Scan() {
		recBytes := scanner.Bytes()
		event, err := parseEvent(recBytes)
		if err != nil {
			return err
		}

		if roleFilter != "" && event.Kind == "response_item" && event.Role != roleFilter {
			continue
		}

		if err := fn(event); err != nil {
			return err
		}
	}

	if err := scanner.Err(); err != nil {
		return fmt.Errorf("scan session: %w", err)
	}

	return nil
}

// buildSummaryText concatenates the first content block texts.
func buildSummaryText(blocks []model.ContentBlock) string {
	if len(blocks) == 0 {
		return ""
	}

	var builder strings.Builder
	for _, block := range blocks {
		if block.Text == "" {
			continue
		}
		if builder.Len() > 0 {
			builder.WriteRune(' ')
		}
		builder.WriteString(strings.TrimSpace(block.Text))
		if builder.Len() >= 160 {
			break
		}
	}

	return builder.String()
}

func newScanner(file *os.File) *bufio.Scanner {
	scanner := bufio.NewScanner(file)
	// Allow large payloads such as instructions blocks.
	const maxCapacity = 8 * 1024 * 1024
	buf := make([]byte, 1024)
	scanner.Buffer(buf, maxCapacity)
	return scanner
}

type rawRecord struct {
	Timestamp string          `json:"timestamp"`
	Type      string          `json:"type"`
	Payload   json.RawMessage `json:"payload"`
}

type sessionMetaPayload struct {
	ID         string `json:"id"`
	Timestamp  string `json:"timestamp"`
	CWD        string `json:"cwd"`
	Originator string `json:"originator"`
	CLIVersion string `json:"cli_version"`
}

type responsePayload struct {
	Type    string          `json:"type"`
	Role    string          `json:"role"`
	Content json.RawMessage `json:"content"`
}

type contentBlock struct {
	Type string `json:"type"`
	Text string `json:"text"`
}

type legacyMeta struct {
	ID         string `json:"id"`
	Timestamp  string `json:"timestamp"`
	CWD        string `json:"cwd"`
	Originator string `json:"originator"`
	CLIVersion string `json:"cli_version"`
}

func tryParseMeta(raw []byte) (*model.SessionMeta, bool, error) {
	event, err := parseEvent(raw)
	if err != nil {
		return nil, false, err
	}

	if event.Kind != "session_meta" {
		legacy := legacyMeta{}
		if err := json.Unmarshal(raw, &legacy); err == nil && legacy.ID != "" {
			tsValue := legacy.Timestamp
			if tsValue == "" {
				tsValue = event.Timestamp.Format(time.RFC3339Nano)
			}
			start, err := parseTimestamp(tsValue)
			if err != nil {
				return nil, false, err
			}
			meta := &model.SessionMeta{
				ID:         legacy.ID,
				CWD:        legacy.CWD,
				Originator: legacy.Originator,
				CLIVersion: legacy.CLIVersion,
				StartedAt:  start,
			}
			return meta, true, nil
		}
		return nil, false, nil
	}

	// Reparse payload for precise fields.
	var rec rawRecord
	if err := json.Unmarshal(raw, &rec); err != nil {
		return nil, false, fmt.Errorf("unmarshal raw meta: %w", err)
	}

	var payload sessionMetaPayload
	if err := json.Unmarshal(rec.Payload, &payload); err != nil {
		return nil, false, fmt.Errorf("unmarshal session_meta payload: %w", err)
	}

	tsValue := payload.Timestamp
	if tsValue == "" {
		tsValue = rec.Timestamp
	}

	start, err := parseTimestamp(tsValue)
	if err != nil {
		return nil, false, err
	}

	meta := &model.SessionMeta{
		ID:         payload.ID,
		CWD:        payload.CWD,
		Originator: payload.Originator,
		CLIVersion: payload.CLIVersion,
		StartedAt:  start,
	}

	return meta, true, nil
}

func parseEvent(raw []byte) (model.Event, error) {
	var rec rawRecord
	if err := json.Unmarshal(raw, &rec); err != nil {
		return model.Event{}, fmt.Errorf("unmarshal record: %w", err)
	}

	var ts time.Time
	if rec.Timestamp != "" {
		var err error
		ts, err = parseTimestamp(rec.Timestamp)
		if err != nil {
			return model.Event{}, err
		}
	}

	event := model.Event{
		Timestamp: ts,
		Kind:      rec.Type,
		Raw:       string(raw),
	}

	switch rec.Type {
	case "session_meta":
		var payload sessionMetaPayload
		if err := json.Unmarshal(rec.Payload, &payload); err != nil {
			return model.Event{}, fmt.Errorf("unmarshal session_meta payload: %w", err)
		}
		event.MessageType = payload.Originator
		event.Content = []model.ContentBlock{
			{Type: "id", Text: payload.ID},
		}
	case "response_item":
		var payload responsePayload
		if err := json.Unmarshal(rec.Payload, &payload); err != nil {
			return model.Event{}, fmt.Errorf("unmarshal response payload: %w", err)
		}
		event.Role = payload.Role
		event.MessageType = payload.Type
		event.Content = decodeContentBlocks(payload.Content)
	default:
		// Pass through unknown payloads as raw JSON.
		event.Content = decodeContentBlocks(rec.Payload)
	}

	return event, nil
}

func decodeContentBlocks(raw json.RawMessage) []model.ContentBlock {
	if len(raw) == 0 {
		return nil
	}

	var array []contentBlock
	if err := json.Unmarshal(raw, &array); err == nil {
		blocks := make([]model.ContentBlock, 0, len(array))
		for _, item := range array {
			blocks = append(blocks, model.ContentBlock{
				Type: item.Type,
				Text: item.Text,
			})
		}
		return blocks
	}

	// Fallback to string representation.
	var asString string
	if err := json.Unmarshal(raw, &asString); err == nil {
		return []model.ContentBlock{{Type: "text", Text: asString}}
	}

	return []model.ContentBlock{{Type: "json", Text: string(raw)}}
}

func parseTimestamp(value string) (time.Time, error) {
	if value == "" {
		return time.Time{}, errors.New("missing timestamp")
	}

	if ts, err := time.Parse(time.RFC3339Nano, value); err == nil {
		return ts, nil
	}
	return time.Parse(time.RFC3339, value)
}
