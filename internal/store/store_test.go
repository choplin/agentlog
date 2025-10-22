package store

import (
	"path/filepath"
	"testing"
	"time"
)

func TestListSessions(t *testing.T) {
	root := filepath.Join("..", "..", "testdata", "sessions")

	res, err := ListSessions(ListOptions{Root: root, MaxSummary: 80})
	if err != nil {
		t.Fatalf("ListSessions returned error: %v", err)
	}

	if len(res.Summaries) != 2 {
		t.Fatalf("expected 2 summaries, got %d", len(res.Summaries))
	}

	if res.Summaries[0].ID != "new-session" {
		t.Fatalf("expected newest session first, got %s", res.Summaries[0].ID)
	}

	if res.Summaries[1].ID != "legacy-session" {
		t.Fatalf("unexpected second session: %s", res.Summaries[1].ID)
	}

	if res.Summaries[0].Summary != "show status" {
		t.Fatalf("unexpected summary text: %s", res.Summaries[0].Summary)
	}

	if len(res.Warnings) != 0 {
		t.Fatalf("expected no warnings, got %d", len(res.Warnings))
	}
}

func TestListSessionsFilters(t *testing.T) {
	root := filepath.Join("..", "..", "testdata", "sessions")
	after := time.Date(2025, 9, 1, 0, 0, 0, 0, time.UTC)

	res, err := ListSessions(ListOptions{Root: root, After: &after})
	if err != nil {
		t.Fatalf("ListSessions returned error: %v", err)
	}

	if len(res.Summaries) != 1 {
		t.Fatalf("expected 1 summary, got %d", len(res.Summaries))
	}

	if res.Summaries[0].ID != "new-session" {
		t.Fatalf("unexpected session id: %s", res.Summaries[0].ID)
	}
}

func TestFindSessionPath(t *testing.T) {
	root := filepath.Join("..", "..", "testdata", "sessions")
	path, err := FindSessionPath(root, "legacy-session")
	if err != nil {
		t.Fatalf("FindSessionPath returned error: %v", err)
	}

	expected := filepath.Join(root, "legacy", "sample.jsonl")
	if path != expected {
		t.Fatalf("unexpected path: %s", path)
	}
}

func TestListSessionsExactCWD(t *testing.T) {
	root := filepath.Join("..", "..", "testdata", "sessions")
	res, err := ListSessions(ListOptions{Root: root, CWD: "/tmp/project", ExactCWD: true})
	if err != nil {
		t.Fatalf("ListSessions returned error: %v", err)
	}

	if len(res.Summaries) != 1 {
		t.Fatalf("expected 1 summary, got %d", len(res.Summaries))
	}

	if res.Summaries[0].ID != "new-session" {
		t.Fatalf("unexpected session id: %s", res.Summaries[0].ID)
	}
}
