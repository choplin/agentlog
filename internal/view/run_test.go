package view

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"codexlog/internal/model"
)

func TestBuildViewFiltersDefaults(t *testing.T) {
	filters, err := buildViewFilters(false, "", "", "")
	if err != nil {
		t.Fatalf("buildViewFilters returned error: %v", err)
	}
	if filters.entryTypes == nil || len(filters.entryTypes) != 1 {
		t.Fatalf("expected default entry type filter with single value, got %#v", filters.entryTypes)
	}
	if _, ok := filters.entryTypes[model.EntryTypeResponseItem]; !ok {
		t.Fatalf("default entry type should include response_item")
	}
	if filters.payloadTypes == nil || len(filters.payloadTypes) != 1 {
		t.Fatalf("expected default payload type filter with single value, got %#v", filters.payloadTypes)
	}
	if _, ok := filters.payloadTypes[model.PayloadTypeMessage]; !ok {
		t.Fatalf("default payload type should include message")
	}
	if filters.payloadRoles == nil || len(filters.payloadRoles) != 2 {
		t.Fatalf("expected default payload roles for user/assistant, got %#v", filters.payloadRoles)
	}
}

func TestEventMatchesFilters(t *testing.T) {
	filters := viewFilters{
		entryTypes: map[model.EntryType]struct{}{
			model.EntryTypeResponseItem: {},
		},
		payloadRoles: map[model.PayloadRole]struct{}{
			model.PayloadRoleAssistant: {},
		},
	}

	event := model.Event{Kind: model.EntryTypeResponseItem, Role: model.PayloadRoleAssistant}
	if !eventMatchesFilters(event, filters) {
		t.Fatalf("expected event to match filters")
	}

	event.Role = model.PayloadRoleUser
	if eventMatchesFilters(event, filters) {
		t.Fatalf("unexpected match for user role")
	}

	event.Kind = model.EntryTypeSessionMeta
	if eventMatchesFilters(event, filters) {
		t.Fatalf("session_meta should be filtered out")
	}
}

func TestParsePayloadRoleArgUnknown(t *testing.T) {
	if _, _, err := parsePayloadRoleArg("user,unknown"); err == nil {
		t.Fatalf("expected error for unknown payload role")
	}
}

func TestRenderChatLinesAlignment(t *testing.T) {
	events := []model.Event{
		{
			Role:      model.PayloadRoleUser,
			Timestamp: time.Date(2025, 10, 27, 12, 0, 0, 0, time.UTC),
			Content: []model.ContentBlock{{Type: "text", Text: "hello there"}},
		},
		{
			Role:      model.PayloadRoleAssistant,
			Timestamp: time.Date(2025, 10, 27, 12, 0, 5, 0, time.UTC),
			Content: []model.ContentBlock{{Type: "text", Text: "hi, how can I help you today?"}},
		},
		{
			Role:      model.PayloadRoleTool,
			Timestamp: time.Date(2025, 10, 27, 12, 0, 10, 0, time.UTC),
			Content: []model.ContentBlock{{Type: "json", Text: `{"result":"ok"}`}},
		},
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
	path := filepath.Join("..", "..", "testdata", "sessions", "new", "sample.jsonl")
	var buf bytes.Buffer
	opts := Options{
		Path:           path,
		Format:         "raw",
		Out:            &buf,
		EntryTypeArg:   "",
		PayloadTypeArg: "",
		PayloadRoleArg: "",
	}
	if err := Run(opts); err != nil {
		t.Fatalf("Run returned error: %v", err)
	}
	wantBytes, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read sample file: %v", err)
	}
	var wantLines []string
	for _, line := range strings.Split(strings.TrimRight(string(wantBytes), "\n"), "\n") {
		if strings.Contains(line, "\"type\":\"session_meta\"") {
			continue
		}
		if strings.TrimSpace(line) == "" {
			continue
		}
		wantLines = append(wantLines, line)
	}
	want := strings.Join(wantLines, "\n") + "\n"
	if buf.String() != want {
		t.Fatalf("raw output mismatch\nwant:\n%q\n\ngot:\n%q", want, buf.String())
	}
}
