package claude

import (
	"agentlog/internal/model"
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"time"
)

// ClaudeParser implements the model.Parser interface for Claude Code logs.
type ClaudeParser struct{}

// Ensure ClaudeParser implements model.Parser
var _ model.Parser = (*ClaudeParser)(nil)

func init() {
	model.RegisterClaudeParser(func() model.Parser {
		return &ClaudeParser{}
	})
}

// ErrSessionMetaNotFound is returned when a JSONL file has no valid entries.
var ErrSessionMetaNotFound = errors.New("no valid entries found in session file")

// ReadSessionMeta loads metadata from the first entry in a Claude Code session file.
// This is the implementation of model.Parser.ReadSessionMeta.
func (p *ClaudeParser) ReadSessionMeta(path string) (model.SessionMetaProvider, error) {
	return ReadSessionMeta(path)
}

// FirstUserSummary extracts the first user message or summary from the session.
// This is the implementation of model.Parser.FirstUserSummary.
func (p *ClaudeParser) FirstUserSummary(path string) (string, error) {
	summary, _, _, err := FirstUserSummary(path)
	return summary, err
}

// IterateEvents iterates through all events in the session.
// This is the implementation of model.Parser.IterateEvents.
func (p *ClaudeParser) IterateEvents(path string, fn func(model.EventProvider) error) error {
	return IterateEvents(path, func(event ClaudeEvent) error {
		return fn(&event)
	})
}

// ReadSessionMeta loads metadata from the first entry in a Claude Code session file.
func ReadSessionMeta(path string) (*ClaudeSessionMeta, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close() //nolint:errcheck

	scanner := newScanner(file)
	for scanner.Scan() {
		recBytes := scanner.Bytes()
		event, err := parseEvent(recBytes)
		if err != nil {
			continue // Skip invalid entries
		}

		// Extract metadata from first valid entry
		if event.Timestamp.IsZero() {
			continue
		}

		meta := &ClaudeSessionMeta{
			ID:        event.SessionID,
			Path:      path,
			CWD:       event.CWD,
			Version:   event.Version,
			StartedAt: event.Timestamp,
		}
		return meta, nil
	}

	if err := scanner.Err(); err != nil {
		return nil, fmt.Errorf("scan session: %w", err)
	}

	return nil, ErrSessionMetaNotFound
}

// FirstUserSummary returns the first user message text and total message count.
func FirstUserSummary(path string) (summary string, messageCount int, lastTimestamp time.Time, err error) {
	file, err := os.Open(path)
	if err != nil {
		return "", 0, time.Time{}, fmt.Errorf("open session file: %w", err)
	}
	defer file.Close() //nolint:errcheck

	scanner := newScanner(file)
	for scanner.Scan() {
		recBytes := scanner.Bytes()
		event, err := parseEvent(recBytes)
		if err != nil {
			continue
		}

		if !event.Timestamp.IsZero() && event.Timestamp.After(lastTimestamp) {
			lastTimestamp = event.Timestamp
		}

		if event.Kind == EntryTypeUser || event.Kind == EntryTypeAssistant {
			messageCount++
			if summary == "" && event.Kind == EntryTypeUser {
				summary = buildSummaryText(event.Content)
			}
		}

		// Also check for summary entries
		if summary == "" && event.Kind == EntryTypeSummary && event.SummaryText != "" {
			summary = event.SummaryText
		}
	}

	if err := scanner.Err(); err != nil {
		return summary, messageCount, lastTimestamp, fmt.Errorf("scan session: %w", err)
	}

	return summary, messageCount, lastTimestamp, nil
}

// IterateEvents walks through the session JSONL file and calls fn for each decoded event.
func IterateEvents(path string, fn func(ClaudeEvent) error) error {
	file, err := os.Open(path)
	if err != nil {
		return fmt.Errorf("open session file: %w", err)
	}
	defer file.Close() //nolint:errcheck

	scanner := newScanner(file)
	for scanner.Scan() {
		recBytes := scanner.Bytes()
		event, err := parseEvent(recBytes)
		if err != nil {
			continue // Skip invalid entries
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
	// Allow large payloads
	const maxCapacity = 8 * 1024 * 1024
	buf := make([]byte, 1024)
	scanner.Buffer(buf, maxCapacity)
	return scanner
}

type rawEntry struct {
	Type       string          `json:"type"`
	UUID       string          `json:"uuid"`
	ParentUUID string          `json:"parentUuid"`
	SessionID  string          `json:"sessionId"`
	CWD        string          `json:"cwd"`
	Version    string          `json:"version"`
	Timestamp  string          `json:"timestamp"`
	Message    json.RawMessage `json:"message"`
	Summary    string          `json:"summary"`
	LeafUUID   string          `json:"leafUuid"`
	IsMeta     bool            `json:"isMeta"`
	UserType   string          `json:"userType"`
}

type messagePayload struct {
	ID      string          `json:"id"`
	Type    string          `json:"type"`
	Role    string          `json:"role"`
	Model   string          `json:"model"`
	Content json.RawMessage `json:"content"`
	Usage   *struct {
		InputTokens              int    `json:"input_tokens"`
		CacheCreationInputTokens int    `json:"cache_creation_input_tokens"`
		CacheReadInputTokens     int    `json:"cache_read_input_tokens"`
		OutputTokens             int    `json:"output_tokens"`
		ServiceTier              string `json:"service_tier"`
	} `json:"usage"`
}

type contentBlock struct {
	Type      string          `json:"type"`
	Text      string          `json:"text"`
	ID        string          `json:"id"`
	Name      string          `json:"name"`
	Input     json.RawMessage `json:"input"`
	ToolUseID string          `json:"tool_use_id"`
	Content   json.RawMessage `json:"content"`
	IsError   bool            `json:"is_error"`
}

func parseEvent(raw []byte) (ClaudeEvent, error) {
	var entry rawEntry
	if err := json.Unmarshal(raw, &entry); err != nil {
		return ClaudeEvent{}, fmt.Errorf("unmarshal entry: %w", err)
	}

	var ts time.Time
	if entry.Timestamp != "" {
		var err error
		ts, err = parseTimestamp(entry.Timestamp)
		if err != nil {
			return ClaudeEvent{}, err
		}
	}

	event := ClaudeEvent{
		Timestamp:  ts,
		Kind:       EntryType(entry.Type),
		UUID:       entry.UUID,
		ParentUUID: entry.ParentUUID,
		SessionID:  entry.SessionID,
		CWD:        entry.CWD,
		Version:    entry.Version,
		IsMeta:     entry.IsMeta,
		UserType:   entry.UserType,
		Raw:        string(raw),
	}

	switch EntryType(entry.Type) {
	case EntryTypeUser, EntryTypeAssistant:
		if len(entry.Message) > 0 {
			var msg messagePayload
			if err := json.Unmarshal(entry.Message, &msg); err != nil {
				return ClaudeEvent{}, fmt.Errorf("unmarshal message: %w", err)
			}

			event.Role = msg.Role
			event.MessageID = msg.ID
			event.Model = msg.Model

			if msg.Usage != nil {
				event.Usage = &TokenUsage{
					InputTokens:              msg.Usage.InputTokens,
					CacheCreationInputTokens: msg.Usage.CacheCreationInputTokens,
					CacheReadInputTokens:     msg.Usage.CacheReadInputTokens,
					OutputTokens:             msg.Usage.OutputTokens,
					ServiceTier:              msg.Usage.ServiceTier,
				}
			}

			event.Content = decodeContent(msg.Content)
		}

	case EntryTypeSummary:
		event.SummaryText = entry.Summary
		event.LeafUUID = entry.LeafUUID
	}

	return event, nil
}

func decodeContent(raw json.RawMessage) []model.ContentBlock {
	if len(raw) == 0 {
		return nil
	}

	// Try as string first (simple message)
	var asString string
	if err := json.Unmarshal(raw, &asString); err == nil {
		// Check if this is a slash command
		if strings.Contains(asString, "<command-name>") && strings.Contains(asString, "</command-name>") {
			return []model.ContentBlock{{Type: "slash_command", Text: asString}}
		}
		// Check if this is a local command output
		if strings.Contains(asString, "<local-command-") || strings.Contains(asString, "<command-message>") {
			return []model.ContentBlock{{Type: "system_message", Text: asString}}
		}
		return []model.ContentBlock{{Type: "text", Text: asString}}
	}

	// Try as array of content blocks
	var blocks []contentBlock
	if err := json.Unmarshal(raw, &blocks); err == nil {
		result := make([]model.ContentBlock, 0, len(blocks))
		for _, block := range blocks {
			switch block.Type {
			case "text":
				result = append(result, model.ContentBlock{
					Type: "text",
					Text: block.Text,
				})
			case "tool_use":
				// Format tool use as readable text
				text := fmt.Sprintf("Tool: %s (ID: %s)", block.Name, block.ID)
				if len(block.Input) > 0 {
					text += fmt.Sprintf("\nInput: %s", string(block.Input))
				}
				result = append(result, model.ContentBlock{
					Type: "tool_use",
					Text: text,
				})
			case "tool_result":
				// Decode nested content in tool_result
				var resultText string
				if len(block.Content) > 0 {
					// Try parsing as array of content blocks
					var nestedBlocks []contentBlock
					if err := json.Unmarshal(block.Content, &nestedBlocks); err == nil {
						var parts []string
						for _, nb := range nestedBlocks {
							if nb.Text != "" {
								parts = append(parts, nb.Text)
							}
						}
						resultText = strings.Join(parts, "\n")
					} else {
						// Fallback to raw content
						resultText = string(block.Content)
					}
				}
				text := fmt.Sprintf("Tool Result (ID: %s)", block.ToolUseID)
				if resultText != "" {
					text += fmt.Sprintf("\n%s", resultText)
				}
				result = append(result, model.ContentBlock{
					Type: "tool_result",
					Text: text,
				})
			default:
				// Unknown type, store as JSON
				result = append(result, model.ContentBlock{
					Type: "json",
					Text: string(raw),
				})
			}
		}
		return result
	}

	// Fallback to JSON representation
	return []model.ContentBlock{{Type: "json", Text: string(raw)}}
}

func parseTimestamp(value string) (time.Time, error) {
	if value == "" {
		return time.Time{}, errors.New("missing timestamp")
	}

	// Try RFC3339Nano first
	if ts, err := time.Parse(time.RFC3339Nano, value); err == nil {
		return ts, nil
	}

	// Try RFC3339
	return time.Parse(time.RFC3339, value)
}
