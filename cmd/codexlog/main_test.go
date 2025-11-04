package main

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"codexlog/internal/model"
)

func TestClipSummary(t *testing.T) {
	if got := clipSummary("abcdef", 3); got != "ab…" {
		t.Fatalf("clipSummary unexpected result: %q", got)
	}
	if got := clipSummary("short", 10); got != "short" {
		t.Fatalf("clipSummary should not alter short text: %q", got)
	}
}

func TestCollapseWhitespace(t *testing.T) {
	text := "  line one\n\nline\t two  "
	if got := collapseWhitespace(text); got != "line one line two" {
		t.Fatalf("collapseWhitespace failed: %q", got)
	}
}

func TestPrintEvent(t *testing.T) {
	event := model.Event{
		Kind:      model.EntryTypeResponseItem,
		Role:      model.PayloadRoleAssistant,
		Timestamp: time.Date(2025, 10, 25, 12, 0, 0, 0, time.UTC),
		Content: []model.ContentBlock{
			{Type: "text", Text: "hello world"},
		},
	}

	var buf bytes.Buffer
	printEvent(&buf, event, 3, 40, false)
	output := buf.String()

	if !strings.Contains(output, "[#003] assistant | 2025-10-25T12:00:00Z") {
		t.Fatalf("header not found in output: %s", output)
	}
	if !strings.Contains(output, "| hello world") {
		t.Fatalf("body not indented correctly: %s", output)
	}
	if !strings.Contains(output, "----------------") {
		t.Fatalf("divider line missing: %s", output)
	}
}

func TestBuildViewFilters_DefaultRoles(t *testing.T) {
	filters, err := buildViewFilters("", "", "")
	if err != nil {
		t.Fatalf("buildViewFilters returned error: %v", err)
	}
	if filters.entryTypes == nil {
		t.Fatalf("expected default entry type filter")
	}
	if len(filters.entryTypes) != 1 {
		t.Fatalf("expected only response_item entry type by default, got %d", len(filters.entryTypes))
	}
	if _, ok := filters.entryTypes[model.EntryTypeResponseItem]; !ok {
		t.Fatalf("default entry type should include response_item")
	}

	if filters.payloadRoles == nil {
		t.Fatalf("expected default payload role filter")
	}
	if len(filters.payloadRoles) != 2 {
		t.Fatalf("expected 2 default roles, got %d", len(filters.payloadRoles))
	}
	if _, ok := filters.payloadRoles[model.PayloadRoleUser]; !ok {
		t.Fatalf("default roles should include user")
	}
	if _, ok := filters.payloadRoles[model.PayloadRoleAssistant]; !ok {
		t.Fatalf("default roles should include assistant")
	}
	if filters.payloadTypes == nil {
		t.Fatalf("expected default payload type filter")
	}
	if len(filters.payloadTypes) != 1 {
		t.Fatalf("expected only message payload type by default, got %d", len(filters.payloadTypes))
	}
	if _, ok := filters.payloadTypes[model.PayloadTypeMessage]; !ok {
		t.Fatalf("default payload type should include message")
	}
}

func TestBuildViewFilters_AllRoles(t *testing.T) {
	filters, err := buildViewFilters("", "", "all")
	if err != nil {
		t.Fatalf("buildViewFilters returned error: %v", err)
	}
	if filters.payloadRoles != nil {
		t.Fatalf("expected nil payloadRoles for 'all' selection")
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

	event := model.Event{
		Kind: model.EntryTypeResponseItem,
		Role: model.PayloadRoleAssistant,
	}
	if !eventMatchesFilters(event, filters) {
		t.Fatalf("expected assistant event to match filters")
	}

	event.Role = model.PayloadRoleUser
	if eventMatchesFilters(event, filters) {
		t.Fatalf("unexpected match for user role with assistant-only filter")
	}

	event.Kind = model.EntryTypeSessionMeta
	if eventMatchesFilters(event, filters) {
		t.Fatalf("session_meta should be excluded by entry type filter")
	}
}

func TestParsePayloadRoleArgUnknown(t *testing.T) {
	if _, _, err := parsePayloadRoleArg("user,unknown"); err == nil {
		t.Fatalf("expected error for unknown payload role")
	}
}

func TestViewCommandFormatRaw(t *testing.T) {
	cmd := newViewCmd()
	var buf bytes.Buffer
	cmd.SetOut(&buf)
	cmd.SetErr(io.Discard)
	path := filepath.Join("..", "..", "testdata", "sessions", "new", "sample.jsonl")
	cmd.SetArgs([]string{path, "--format", "raw"})
	if err := cmd.Execute(); err != nil {
		t.Fatalf("view command failed: %v", err)
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
	if got := buf.String(); got != want {
		t.Fatalf("raw output mismatch\nwant:\n%q\n\ngot:\n%q", want, got)
	}
}
