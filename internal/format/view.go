package format

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"codexlog/internal/model"
)

// RenderEvent converts a session event into a printable string.
func RenderEvent(event model.Event, wrapWidth int) string {
	switch event.Kind {
	case "session_meta":
		return fmt.Sprintf("Session %s (%s)", contentValue(event.Content, "id"), event.Timestamp.Format(time.RFC3339))
	case "response_item":
		body := renderBlocks(event.Content, wrapWidth)
		label := event.Role
		if label == "" {
			label = event.MessageType
		}
		return fmt.Sprintf("[%s][%s]\n%s", event.Timestamp.Format(time.RFC3339), label, body)
	default:
		body := renderBlocks(event.Content, wrapWidth)
		label := event.Kind
		if label == "" {
			label = event.MessageType
		}
		if label == "" {
			label = "event"
		}
		return fmt.Sprintf("[%s][%s]\n%s", event.Timestamp.Format(time.RFC3339), label, body)
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
		case "input_text", "output_text", "text":
			parts = append(parts, wrapBody(strings.TrimSpace(block.Text), wrapWidth))
		case "json":
			parts = append(parts, formatJSON(block.Text))
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
