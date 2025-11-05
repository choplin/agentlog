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
func FirstUserSummary(path string) (summary string, messageCount int, lastTimestamp time.Time, err error) {
	file, err := os.Open(path)
	if err != nil {
		return "", 0, time.Time{}, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close()

	scanner := newScanner(file)
	for scanner.Scan() {
		recBytes := scanner.Bytes()
		event, err := parseEvent(recBytes)
		if err != nil {
			return "", messageCount, lastTimestamp, err
		}

		if !event.Timestamp.IsZero() && event.Timestamp.After(lastTimestamp) {
			lastTimestamp = event.Timestamp
		}

		if event.Kind == model.EntryTypeResponseItem {
			messageCount++
			if summary == "" && event.Role == model.PayloadRoleUser {
				summary = buildSummaryText(event.Content)
			}
		}
	}

	if err := scanner.Err(); err != nil {
		return summary, messageCount, lastTimestamp, fmt.Errorf("scan session: %w", err)
	}

	return summary, messageCount, lastTimestamp, nil
}

// IterateEvents walks through the session JSONL file and calls fn for each
// decoded event.
func IterateEvents(path string, fn func(model.Event) error) error {
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

type functionCallPayload struct {
	Type      string          `json:"type"`
	Role      string          `json:"role"`
	Name      string          `json:"name"`
	Arguments string          `json:"arguments"`
	Output    string          `json:"output"`
	Content   json.RawMessage `json:"content"`
	Summary   json.RawMessage `json:"summary"`
}

type tokenUsage struct {
	InputTokens         int `json:"input_tokens"`
	CachedInputTokens   int `json:"cached_input_tokens"`
	OutputTokens        int `json:"output_tokens"`
	ReasoningTokens     int `json:"reasoning_output_tokens"`
	TotalTokens         int `json:"total_tokens"`
}

type tokenCountInfo struct {
	TotalTokenUsage tokenUsage `json:"total_token_usage"`
	LastTokenUsage  tokenUsage `json:"last_token_usage"`
}

type eventMsgPayload struct {
	Type    string          `json:"type"`
	Content string          `json:"content"`
	Text    string          `json:"text"`
	Message string          `json:"message"`
	Info    *tokenCountInfo `json:"info"`
}

type turnContextPayload struct {
	TurnID          string `json:"turn_id"`
	Context         string `json:"context"`
	CWD             string `json:"cwd"`
	Model           string `json:"model"`
	Effort          string `json:"effort"`
	Summary         string `json:"summary"`
	ApprovalPolicy  string `json:"approval_policy"`
}

func tryParseMeta(raw []byte) (*model.SessionMeta, bool, error) {
	event, err := parseEvent(raw)
	if err != nil {
		return nil, false, err
	}

	if event.Kind != model.EntryTypeSessionMeta {
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

	entryType := model.EntryType(rec.Type)
	event := model.Event{
		Timestamp: ts,
		Kind:      entryType,
		Raw:       string(raw),
	}

	switch entryType {
	case model.EntryTypeSessionMeta:
		var payload sessionMetaPayload
		if err := json.Unmarshal(rec.Payload, &payload); err != nil {
			return model.Event{}, fmt.Errorf("unmarshal session_meta payload: %w", err)
		}
		event.PayloadType = payload.Originator
		event.Content = []model.ContentBlock{
			{Type: "id", Text: payload.ID},
		}
	case model.EntryTypeResponseItem:
		var payload functionCallPayload
		if err := json.Unmarshal(rec.Payload, &payload); err != nil {
			return model.Event{}, fmt.Errorf("unmarshal response payload: %w", err)
		}
		event.Role = model.PayloadRole(payload.Role)
		event.PayloadType = payload.Type

		// Handle function_call and custom_tool_call types
		if payload.Type == "function_call" || payload.Type == "custom_tool_call" {
			if payload.Name != "" {
				event.Content = []model.ContentBlock{
					{Type: "function_name", Text: payload.Name},
					{Type: "function_arguments", Text: payload.Arguments},
				}
			} else {
				event.Content = decodeContentBlocks(payload.Content)
			}
		} else if payload.Type == "function_call_output" || payload.Type == "custom_tool_call_output" {
			// Handle function_call_output and custom_tool_call_output
			if payload.Output != "" {
				event.Content = []model.ContentBlock{
					{Type: "function_output", Text: payload.Output},
				}
			} else {
				event.Content = decodeContentBlocks(payload.Content)
			}
		} else {
			event.Content = decodeContentBlocks(payload.Content)
			// If content is empty or null, try summary (for encrypted reasoning)
			if len(event.Content) == 0 && len(payload.Summary) > 0 {
				event.Content = decodeContentBlocks(payload.Summary)
			}
		}
	case model.EntryTypeEventMsg:
		var payload eventMsgPayload
		if err := json.Unmarshal(rec.Payload, &payload); err != nil {
			return model.Event{}, fmt.Errorf("unmarshal event_msg payload: %w", err)
		}
		event.PayloadType = payload.Type

		// Build content based on event_msg type
		var blocks []model.ContentBlock
		switch payload.Type {
		case "user_message", "agent_message":
			text := payload.Content
			if text == "" {
				text = payload.Message
			}
			if text != "" {
				blocks = append(blocks, model.ContentBlock{Type: "text", Text: text})
			}
		case "token_count":
			if payload.Info != nil {
				usage := payload.Info.TotalTokenUsage
				text := fmt.Sprintf("Tokens: %d in / %d out", usage.InputTokens, usage.OutputTokens)
				if usage.CachedInputTokens > 0 {
					text += fmt.Sprintf(" (%d cached)", usage.CachedInputTokens)
				}
				if usage.ReasoningTokens > 0 {
					text += fmt.Sprintf(" [%d reasoning]", usage.ReasoningTokens)
				}
				blocks = append(blocks, model.ContentBlock{Type: "text", Text: text})
			} else {
				blocks = append(blocks, model.ContentBlock{Type: "text", Text: "Token usage unavailable"})
			}
		case "agent_reasoning":
			if payload.Text != "" {
				blocks = append(blocks, model.ContentBlock{Type: "text", Text: payload.Text})
			}
		case "turn_aborted":
			blocks = append(blocks, model.ContentBlock{Type: "text", Text: "Turn aborted"})
		default:
			// Fallback to JSON for unknown event_msg types
			blocks = decodeContentBlocks(rec.Payload)
		}
		event.Content = blocks
	case model.EntryTypeTurnContext:
		var payload turnContextPayload
		if err := json.Unmarshal(rec.Payload, &payload); err != nil {
			return model.Event{}, fmt.Errorf("unmarshal turn_context payload: %w", err)
		}
		event.PayloadType = "turn_context"

		// Build content based on available fields
		var text string
		if payload.TurnID != "" && payload.Context != "" {
			text = fmt.Sprintf("Turn: %s - %s", payload.TurnID, payload.Context)
		} else {
			// Use model and effort info instead
			parts := []string{}
			if payload.Model != "" {
				parts = append(parts, fmt.Sprintf("Model: %s", payload.Model))
			}
			if payload.Effort != "" {
				parts = append(parts, fmt.Sprintf("Effort: %s", payload.Effort))
			}
			if payload.CWD != "" {
				parts = append(parts, fmt.Sprintf("CWD: %s", payload.CWD))
			}
			if len(parts) > 0 {
				text = strings.Join(parts, ", ")
			} else {
				text = "Turn context"
			}
		}
		event.Content = []model.ContentBlock{
			{Type: "text", Text: text},
		}
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
