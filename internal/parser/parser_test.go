package parser

import (
	"path/filepath"
	"testing"
	"time"

	"codexlog/internal/model"
)

func fixturePath(parts ...string) string {
	elems := append([]string{"..", "..", "testdata", "sessions"}, parts...)
	return filepath.Join(elems...)
}

func TestReadSessionMeta_NewFormat(t *testing.T) {
	path := fixturePath("new", "sample.jsonl")

	meta, err := ReadSessionMeta(path)
	if err != nil {
		t.Fatalf("ReadSessionMeta returned error: %v", err)
	}

	if meta.ID != "new-session" {
		t.Fatalf("unexpected session id: %s", meta.ID)
	}
	if got := meta.StartedAt.Format(time.RFC3339); got != "2025-10-01T12:00:00Z" {
		t.Fatalf("unexpected start time: %s", got)
	}
	if meta.CWD != "/tmp/project" {
		t.Fatalf("unexpected cwd: %s", meta.CWD)
	}
}

func TestReadSessionMeta_LegacyFormat(t *testing.T) {
	path := fixturePath("legacy", "sample.jsonl")

	meta, err := ReadSessionMeta(path)
	if err != nil {
		t.Fatalf("ReadSessionMeta returned error: %v", err)
	}

	if meta.ID != "legacy-session" {
		t.Fatalf("unexpected session id: %s", meta.ID)
	}
	if got := meta.StartedAt.Format(time.RFC3339); got != "2025-08-01T09:00:00Z" {
		t.Fatalf("unexpected start time: %s", got)
	}
	if meta.CWD != "/tmp/legacy" {
		t.Fatalf("unexpected cwd: %s", meta.CWD)
	}
}

func TestFirstUserSummary(t *testing.T) {
	path := fixturePath("new", "sample.jsonl")

	summary, count, err := FirstUserSummary(path)
	if err != nil {
		t.Fatalf("FirstUserSummary returned error: %v", err)
	}

	if summary != "show status" {
		t.Fatalf("unexpected summary: %q", summary)
	}
	if count != 2 {
		t.Fatalf("unexpected message count: %d", count)
	}
}

func TestIterateEvents_Filtered(t *testing.T) {
	path := fixturePath("new", "sample.jsonl")

	var events []string
	err := IterateEvents(path, "assistant", func(evt model.Event) error {
		if evt.Kind == "response_item" {
			events = append(events, evt.Role)
		}
		return nil
	})
	if err != nil {
		t.Fatalf("IterateEvents returned error: %v", err)
	}

	if len(events) != 1 {
		t.Fatalf("expected 1 assistant event, got %d", len(events))
	}
	if events[0] != "assistant" {
		t.Fatalf("unexpected role: %s", events[0])
	}
}
