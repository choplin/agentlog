package format

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"codexlog/internal/model"
)

// RenderEventLines returns the formatted body lines for a session event.
func RenderEventLines(event model.Event, wrapWidth int) []string {
	switch event.Kind {
	case model.EntryTypeSessionMeta:
		return []string{fmt.Sprintf("Session %s (%s)", contentValue(event.Content, "id"), event.Timestamp.Format(time.RFC3339))}
	default:
		body := renderBlocks(event.Content, wrapWidth)
		if body == "" {
			return nil
		}
		return strings.Split(body, "\n")
	}
}

// RenderEvent converts a session event into a printable string (legacy helper).
func RenderEvent(event model.Event, wrapWidth int) string {
	lines := RenderEventLines(event, wrapWidth)
	switch event.Kind {
	case model.EntryTypeSessionMeta:
		return strings.Join(lines, "\n")
	case model.EntryTypeResponseItem:
		label := string(event.Role)
		if label == "" {
			label = event.PayloadType
		}
		return fmt.Sprintf("[%s][%s]\n%s", event.Timestamp.Format(time.RFC3339), label, strings.Join(lines, "\n"))
	default:
		label := string(event.Kind)
		if label == "" {
			label = event.PayloadType
		}
		if label == "" {
			label = "event"
		}
		return fmt.Sprintf("[%s][%s]\n%s", event.Timestamp.Format(time.RFC3339), label, strings.Join(lines, "\n"))
	}
}

// renderBlocks joins content blocks into a printable string with optional wrapping.
func renderBlocks(blocks []model.ContentBlock, wrapWidth int) string {
	if len(blocks) == 0 {
		return ""
	}
	parts := make([]string, 0, len(blocks))
	for _, block := range blocks {
		switch block.Type {
		case "input_text", "output_text", "text", "summary_text":
			parts = append(parts, wrapBody(strings.TrimSpace(block.Text), wrapWidth))
		case "json":
			parts = append(parts, formatJSON(block.Text))
		case "function_name":
			parts = append(parts, fmt.Sprintf("Function: %s", block.Text))
		case "function_arguments":
			// Try to format arguments as JSON if possible
			formatted := formatJSON(block.Text)
			if formatted == block.Text {
				// Not valid JSON, show as-is
				parts = append(parts, fmt.Sprintf("Arguments: %s", block.Text))
			} else {
				parts = append(parts, fmt.Sprintf("Arguments:\n%s", formatted))
			}
		default:
			prefix := fmt.Sprintf("[%s] ", block.Type)
			parts = append(parts, prefix+wrapBody(strings.TrimSpace(block.Text), wrapWidth))
		}
	}
	return strings.Join(parts, "\n")
}

func wrapBody(text string, width int) string {
	if width <= 0 || len(text) <= width {
		return text
	}

	words := strings.Fields(text)
	if len(words) == 0 {
		return ""
	}

	var lines []string
	current := words[0]
	for _, word := range words[1:] {
		if len(current)+1+len(word) > width {
			lines = append(lines, current)
			current = word
		} else {
			current += " " + word
		}
	}
	lines = append(lines, current)

	return strings.Join(lines, "\n")
}

func contentValue(blocks []model.ContentBlock, expected string) string {
	for _, block := range blocks {
		if block.Type == expected {
			return block.Text
		}
	}
	return ""
}

func formatJSON(raw string) string {
	if raw == "" {
		return raw
	}

	var buf bytes.Buffer
	if err := json.Indent(&buf, []byte(raw), "", "  "); err == nil {
		return buf.String()
	}
	return raw
}
