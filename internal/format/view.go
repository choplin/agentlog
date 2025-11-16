package format

import (
	"agentlog/internal/model"
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"time"
)

// RenderEventLines returns the formatted body lines for a session event.
func RenderEventLines(event model.EventProvider, wrapWidth int) []string {
	body := renderBlocks(event.GetContent(), wrapWidth)
	if body == "" {
		return nil
	}
	return strings.Split(body, "\n")
}

// RenderEvent converts a session event into a printable string (legacy helper).
func RenderEvent(event model.EventProvider, wrapWidth int) string {
	lines := RenderEventLines(event, wrapWidth)
	label := event.GetRole()
	if label == "" {
		label = "event"
	}

	ts := "-"
	if !event.GetTimestamp().IsZero() {
		ts = event.GetTimestamp().Format(time.RFC3339)
	}
	return fmt.Sprintf("[%s][%s]\n%s", ts, label, strings.Join(lines, "\n"))
}

// renderBlocks joins content blocks into a printable string with optional wrapping.
func renderBlocks(blocks []model.ContentBlock, wrapWidth int) string {
	if len(blocks) == 0 {
		return ""
	}
	parts := make([]string, 0, len(blocks))
	for _, block := range blocks {
		switch block.Type {
		case "slash_command":
			text := formatSlashCommand(block.Text)
			parts = append(parts, wrapBody(text, wrapWidth))
		case "input_text", "output_text", "text", "summary_text":
			text := strings.TrimSpace(block.Text)
			parts = append(parts, wrapBody(text, wrapWidth))
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
		case "function_output":
			// Try to format output as JSON if possible
			formatted := formatJSON(block.Text)
			if formatted == block.Text {
				// Not valid JSON, show as-is
				parts = append(parts, fmt.Sprintf("Output: %s", block.Text))
			} else {
				parts = append(parts, fmt.Sprintf("Output:\n%s", formatted))
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

// formatSlashCommand extracts and formats slash command information from XML-like tags.
func formatSlashCommand(text string) string {
	// Extract command name
	commandName := extractTagContent(text, "command-name")
	commandMessage := extractTagContent(text, "command-message")

	if commandName != "" {
		if commandMessage != "" {
			return fmt.Sprintf("[Slash Command: %s] %s", commandName, commandMessage)
		}
		return fmt.Sprintf("[Slash Command: %s]", commandName)
	}

	// If we can't parse it cleanly, return as-is
	return text
}

// extractTagContent extracts content between <tag> and </tag>.
func extractTagContent(text, tag string) string {
	startTag := "<" + tag + ">"
	endTag := "</" + tag + ">"

	startIdx := strings.Index(text, startTag)
	if startIdx == -1 {
		return ""
	}
	startIdx += len(startTag)

	endIdx := strings.Index(text[startIdx:], endTag)
	if endIdx == -1 {
		return ""
	}

	return text[startIdx : startIdx+endIdx]
}
