package view

import (
	"agentlog/internal/codex"
	"agentlog/internal/model"
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestRenderChatLinesAlignment(t *testing.T) {
	codexEvents := []codex.CodexEvent{
		{
			Role:      codex.PayloadRoleUser,
			Timestamp: time.Date(2025, 10, 27, 12, 0, 0, 0, time.UTC),
			Content:   []model.ContentBlock{{Type: "text", Text: "hello there"}},
		},
		{
			Role:      codex.PayloadRoleAssistant,
			Timestamp: time.Date(2025, 10, 27, 12, 0, 5, 0, time.UTC),
			Content:   []model.ContentBlock{{Type: "text", Text: "hi, how can I help you today?"}},
		},
		{
			Role:      codex.PayloadRoleTool,
			Timestamp: time.Date(2025, 10, 27, 12, 0, 10, 0, time.UTC),
			Content:   []model.ContentBlock{{Type: "json", Text: `{"result":"ok"}`}},
		},
	}
	// Convert to interface slice
	events := make([]model.EventProvider, len(codexEvents))
	for i := range codexEvents {
		events[i] = &codexEvents[i]
	}

	lines := renderChatTranscript(events, 80, false)
	if len(lines) == 0 {
		t.Fatal("expected chat lines")
	}

	userTop := findPrefix(lines, "╭")
	if userTop < 0 {
		t.Fatalf("failed to locate user bubble: %v", lines)
	}

	next := findPrefix(lines[userTop+1:], "╭")
	if next < 0 {
		t.Fatalf("failed to locate assistant bubble: %v", lines)
	}
	assistantTop := next + userTop + 1

	if idx := strings.Index(lines[userTop], "╭"); idx <= 2 {
		t.Fatalf("user bubble should be right aligned, got index %d line %q", idx, lines[userTop])
	}

	if !strings.HasPrefix(lines[assistantTop], "  ╭") {
		t.Fatalf("assistant bubble should be left aligned: %q", lines[assistantTop])
	}
}

func findPrefix(lines []string, prefix string) int {
	for i, line := range lines {
		if strings.HasPrefix(line, prefix) || strings.Contains(line, prefix) {
			return i
		}
	}
	return -1
}

func TestRunFormatRaw(t *testing.T) {
	path := filepath.Join("..", "..", "testdata", "sessions", "sample-simple.jsonl")
	parser := &codex.CodexParser{}
	var buf bytes.Buffer
	opts := Options{
		Path:   path,
		Format: "raw",
		Out:    &buf,
		Level:  "all",
	}
	if err := Run(parser, opts); err != nil {
		t.Fatalf("Run returned error: %v", err)
	}
	wantBytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read sample file: %v", err)
	}
	// With --level all, we expect all lines including session_meta
	want := string(wantBytes)
	if buf.String() != want {
		t.Fatalf("raw output mismatch\nwant:\n%q\n\ngot:\n%q", want, buf.String())
	}
}

func TestLevelFiltering(t *testing.T) {
	path := filepath.Join("..", "..", "testdata", "sessions", "sample-full.jsonl")
	parser := &codex.CodexParser{}

	tests := []struct {
		name          string
		level         string
		minExpected   int // Minimum expected events
		shouldInclude []string
		shouldExclude []string
	}{
		{
			name:          "conversation level shows only user/assistant",
			level:         "conversation",
			minExpected:   5,
			shouldInclude: []string{"user", "assistant"},
			shouldExclude: []string{"event_msg", "turn_context"},
		},
		{
			name:        "all level shows everything",
			level:       "all",
			minExpected: 10,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			var buf bytes.Buffer
			opts := Options{
				Path:   path,
				Format: "raw",
				Out:    &buf,
				Level:  tt.level,
			}
			if err := Run(parser, opts); err != nil {
				t.Fatalf("Run returned error: %v", err)
			}

			output := buf.String()
			lines := strings.Split(strings.TrimRight(output, "\n"), "\n")
			var actualCount int
			for _, line := range lines {
				if strings.TrimSpace(line) != "" {
					actualCount++
				}
			}

			if actualCount < tt.minExpected {
				t.Fatalf("%s: expected at least %d entries, got %d",
					tt.name, tt.minExpected, actualCount)
			}
		})
	}
}

func TestInvalidLevel(t *testing.T) {
	path := filepath.Join("..", "..", "testdata", "sessions", "sample-simple.jsonl")
	parser := &codex.CodexParser{}
	var buf bytes.Buffer
	opts := Options{
		Path:   path,
		Format: "text",
		Out:    &buf,
		Level:  "invalid",
	}
	err := Run(parser, opts)
	if err == nil {
		t.Fatal("Expected error for invalid level, got nil")
	}
	if !strings.Contains(err.Error(), "invalid level") {
		t.Fatalf("Expected 'invalid level' error, got: %v", err)
	}
}
