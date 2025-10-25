package format

import (
	"bytes"
	"strings"
	"testing"
	"time"

	"codexlog/internal/model"
)

func sampleSummaries() []model.SessionSummary {
	return []model.SessionSummary{
		{
			ID:              "session-a",
			CWD:             "/tmp/project",
			StartedAt:       time.Date(2025, 10, 1, 12, 0, 0, 0, time.UTC),
			Summary:         "Alpha",
			MessageCount:    10,
			DurationSeconds: 90,
		},
		{
			ID:              "session-b",
			CWD:             "/tmp/other",
			StartedAt:       time.Date(2025, 10, 2, 9, 30, 0, 0, time.UTC),
			Summary:         "Beta",
			MessageCount:    20,
			DurationSeconds: 45,
		},
	}
}

func TestWriteSummariesPlain(t *testing.T) {
	var buf bytes.Buffer
	items := sampleSummaries()

	if err := WriteSummaries(&buf, items, true, "plain"); err != nil {
		t.Fatalf("WriteSummaries plain returned error: %v", err)
	}

	expected := strings.Join([]string{
		"timestamp\tsession_id\tcwd\tduration\tmessage_count\tsummary",
		"2025-10-01T12:00:00Z\tsession-a\t/tmp/project\t00:01:30\t10\tAlpha",
		"2025-10-02T09:30:00Z\tsession-b\t/tmp/other\t00:00:45\t20\tBeta",
	}, "\n") + "\n"

	if got := buf.String(); got != expected {
		t.Fatalf("plain output mismatch:\nexpected: %q\nactual:   %q", expected, got)
	}
}

func TestWriteSummariesTable(t *testing.T) {
	var buf bytes.Buffer
	items := sampleSummaries()

	if err := WriteSummaries(&buf, items, true, "table"); err != nil {
		t.Fatalf("WriteSummaries table returned error: %v", err)
	}

	out := buf.String()
	if !strings.Contains(out, "DURATION") || !strings.Contains(out, "MESSAGES") {
		t.Fatalf("table header missing expected columns:\n%s", out)
	}

	if !strings.Contains(out, "│ 2025-10-01T12:00:00Z │ session-a  │ /tmp/project │ 00:01:30 │       10 │ Alpha   │") {
		t.Fatalf("table row order unexpected: %s", out)
	}
}

func TestWriteSummariesInvalidFormat(t *testing.T) {
	var buf bytes.Buffer
	err := WriteSummaries(&buf, sampleSummaries(), true, "xml")
	if err == nil {
		t.Fatal("expected error for unsupported format")
	}
}

func TestWriteSummariesJSONL(t *testing.T) {
	var buf bytes.Buffer
	items := sampleSummaries()

	if err := WriteSummaries(&buf, items, false, "jsonl"); err != nil {
		t.Fatalf("WriteSummaries jsonl returned error: %v", err)
	}

	lines := strings.Split(strings.TrimSpace(buf.String()), "\n")
	if len(lines) != len(items) {
		t.Fatalf("expected %d lines, got %d", len(items), len(lines))
	}
	if !strings.Contains(lines[0], "\"session-a\"") || !strings.Contains(lines[0], "\"DurationSeconds\":90") {
		t.Fatalf("first jsonl line unexpected: %s", lines[0])
	}
}
