// Package store provides session enumeration and search functionality.
package store

import (
	"agentlog/internal/model"
	"errors"
	"fmt"
	"io/fs"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

var errStop = errors.New("stop iteration")

// sessionSummary implements model.SessionSummaryProvider.
type sessionSummary struct {
	id              string
	path            string
	cwd             string
	startedAt       time.Time
	summary         string
	messageCount    int
	durationSeconds int
}

func (s *sessionSummary) GetID() string           { return s.id }
func (s *sessionSummary) GetPath() string         { return s.path }
func (s *sessionSummary) GetCWD() string          { return s.cwd }
func (s *sessionSummary) GetStartedAt() time.Time { return s.startedAt }
func (s *sessionSummary) GetSummary() string      { return s.summary }
func (s *sessionSummary) GetMessageCount() int    { return s.messageCount }
func (s *sessionSummary) GetDurationSeconds() int { return s.durationSeconds }

// ListOptions controls how sessions are enumerated.
type ListOptions struct {
	Root       string
	CWD        string
	ExactCWD   bool
	After      *time.Time
	Before     *time.Time
	Limit      int
	MaxSummary int
}

// ListResult contains session summaries and non-fatal warnings.
type ListResult struct {
	Summaries []model.SessionSummaryProvider
	Warnings  []error
}

// ListSessions enumerates sessions under Root according to options using the provided parser.
func ListSessions(parser model.Parser, opts ListOptions) (ListResult, error) {
	root := opts.Root
	if root == "" {
		return ListResult{}, errors.New("root directory is required")
	}

	var result ListResult

	err := filepath.WalkDir(root, func(path string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			result.Warnings = append(result.Warnings, fmt.Errorf("walk %s: %w", path, walkErr))
			return nil
		}

		if d.IsDir() || !strings.HasSuffix(d.Name(), ".jsonl") {
			return nil
		}

		meta, err := parser.ReadSessionMeta(path)
		if err != nil {
			result.Warnings = append(result.Warnings, fmt.Errorf("parse meta %s: %w", path, err))
			return nil
		}

		if opts.CWD != "" {
			if opts.ExactCWD {
				if meta.GetCWD() != opts.CWD {
					return nil
				}
			} else if !strings.HasPrefix(meta.GetCWD(), opts.CWD) {
				return nil
			}
		}
		if opts.After != nil && meta.GetStartedAt().Before(*opts.After) {
			return nil
		}
		if opts.Before != nil && meta.GetStartedAt().After(*opts.Before) {
			return nil
		}

		summaryText, err := parser.FirstUserSummary(path)
		if err != nil {
			result.Warnings = append(result.Warnings, fmt.Errorf("extract summary %s: %w", path, err))
			return nil
		}

		if opts.MaxSummary > 0 && len(summaryText) > opts.MaxSummary {
			summaryText = truncate(summaryText, opts.MaxSummary)
		}

		// Count messages and find last timestamp
		var count int
		var lastTimestamp time.Time
		err = parser.IterateEvents(path, func(event model.EventProvider) error {
			count++
			if !event.GetTimestamp().IsZero() && event.GetTimestamp().After(lastTimestamp) {
				lastTimestamp = event.GetTimestamp()
			}
			return nil
		})
		if err != nil {
			result.Warnings = append(result.Warnings, fmt.Errorf("count messages %s: %w", path, err))
			return nil
		}

		if lastTimestamp.IsZero() || lastTimestamp.Before(meta.GetStartedAt()) {
			lastTimestamp = meta.GetStartedAt()
		}

		duration := durationSeconds(meta.GetStartedAt(), lastTimestamp)

		result.Summaries = append(result.Summaries, &sessionSummary{
			id:              meta.GetID(),
			path:            path,
			cwd:             meta.GetCWD(),
			startedAt:       meta.GetStartedAt(),
			summary:         summaryText,
			messageCount:    count,
			durationSeconds: duration,
		})

		return nil
	})
	if err != nil {
		return result, err
	}

	sort.Slice(result.Summaries, func(i, j int) bool {
		return result.Summaries[i].GetStartedAt().After(result.Summaries[j].GetStartedAt())
	})

	if opts.Limit > 0 && len(result.Summaries) > opts.Limit {
		result.Summaries = result.Summaries[:opts.Limit]
	}

	return result, nil
}

func truncate(s string, maxLen int) string {
	if len(s) <= maxLen {
		return s
	}
	runes := []rune(s)
	if len(runes) <= maxLen {
		return s
	}
	return string(runes[:maxLen]) + "…"
}

// FindSessionPath searches for a session file whose session id matches id.
func FindSessionPath(parser model.Parser, root, id string) (string, error) {
	if root == "" {
		return "", errors.New("root directory is required")
	}
	if id == "" {
		return "", errors.New("session id is required")
	}

	var matched string
	err := filepath.WalkDir(root, func(path string, d fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return nil
		}
		if d.IsDir() || !strings.HasSuffix(d.Name(), ".jsonl") {
			return nil
		}
		meta, err := parser.ReadSessionMeta(path)
		if err != nil {
			return nil
		}
		if meta.GetID() == id {
			matched = path
			return errStop
		}
		return nil
	})

	if matched != "" {
		return matched, nil
	}
	if err != nil {
		if errors.Is(err, errStop) {
			return matched, nil
		}
		return "", err
	}
	return "", fmt.Errorf("session id %s not found under %s", id, root)
}

func durationSeconds(start, end time.Time) int {
	if start.IsZero() || end.IsZero() {
		return 0
	}
	if end.Before(start) {
		return 0
	}
	return int(end.Sub(start).Seconds())
}
