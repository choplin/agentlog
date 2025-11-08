package claude

import (
	"os"
	"path/filepath"
	"testing"
)

func TestRealClaudeCodeLog(t *testing.T) {
	// Skip if file doesn't exist
	home, err := os.UserHomeDir()
	if err != nil {
		t.Skip("Cannot get home directory")
	}

	path := filepath.Join(home, ".claude", "projects", "-Users-aki-workspace-codexlog", "408f0b34-6006-41da-9400-ce3574f77359.jsonl")
	if _, err := os.Stat(path); os.IsNotExist(err) {
		t.Skip("Real log file not found")
	}

	t.Run("ReadSessionMeta", func(t *testing.T) {
		meta, err := ReadSessionMeta(path)
		if err != nil {
			t.Fatalf("ReadSessionMeta error: %v", err)
		}

		t.Logf("Session Meta:")
		t.Logf("  ID: %s", meta.ID)
		t.Logf("  CWD: %s", meta.CWD)
		t.Logf("  Version: %s", meta.Version)
		t.Logf("  StartedAt: %s", meta.StartedAt)

		if meta.ID == "" {
			t.Error("Expected non-empty session ID")
		}
	})

	t.Run("FirstUserSummary", func(t *testing.T) {
		summary, count, last, err := FirstUserSummary(path)
		if err != nil {
			t.Fatalf("FirstUserSummary error: %v", err)
		}

		t.Logf("Summary:")
		t.Logf("  First message: %s", summary)
		t.Logf("  Message count: %d", count)
		t.Logf("  Last timestamp: %s", last)

		if summary == "" {
			t.Error("Expected non-empty summary")
		}
		if count == 0 {
			t.Error("Expected non-zero message count")
		}
	})

	t.Run("IterateEvents", func(t *testing.T) {
		eventCount := 0
		userCount := 0
		assistantCount := 0
		summaryCount := 0
		otherTypes := make(map[EntryType]int)

		err := IterateEvents(path, func(evt ClaudeEvent) error {
			eventCount++
			switch evt.Kind {
			case EntryTypeUser:
				userCount++
			case EntryTypeAssistant:
				assistantCount++
			case EntryTypeSummary:
				summaryCount++
			default:
				otherTypes[evt.Kind]++
			}

			// Log first 3 events
			if eventCount <= 3 {
				t.Logf("Event %d: Type=%s, Role=%s, Content=%d blocks",
					eventCount, evt.Kind, evt.Role, len(evt.Content))
			}

			return nil
		})
		if err != nil {
			t.Fatalf("IterateEvents error: %v", err)
		}

		t.Logf("Event statistics:")
		t.Logf("  Total: %d", eventCount)
		t.Logf("  User: %d", userCount)
		t.Logf("  Assistant: %d", assistantCount)
		t.Logf("  Summary: %d", summaryCount)
		if len(otherTypes) > 0 {
			t.Logf("  Other types: %v", otherTypes)
		}
	})
}
