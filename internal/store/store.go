package store

import (
	"errors"
	"fmt"
	"io/fs"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"codexlog/internal/model"
	"codexlog/internal/parser"
)

var errStop = errors.New("stop iteration")

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
	Summaries []model.SessionSummary
	Warnings  []error
}

// ListSessions enumerates Codex sessions under Root according to options.
func ListSessions(opts ListOptions) (ListResult, error) {
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
		meta.Path = path

		if opts.CWD != "" {
			if opts.ExactCWD {
				if meta.CWD != opts.CWD {
					return nil
				}
			} else if !strings.HasPrefix(meta.CWD, opts.CWD) {
				return nil
			}
		}
		if opts.After != nil && meta.StartedAt.Before(*opts.After) {
			return nil
		}
		if opts.Before != nil && meta.StartedAt.After(*opts.Before) {
			return nil
		}

		summaryText, count, lastTimestamp, err := parser.FirstUserSummary(path)
		if err != nil {
			result.Warnings = append(result.Warnings, fmt.Errorf("extract summary %s: %w", path, err))
			return nil
		}

		if opts.MaxSummary > 0 && len(summaryText) > opts.MaxSummary {
			summaryText = truncate(summaryText, opts.MaxSummary)
		}

		if lastTimestamp.IsZero() || lastTimestamp.Before(meta.StartedAt) {
			lastTimestamp = meta.StartedAt
		}

		duration := durationSeconds(meta.StartedAt, lastTimestamp)

		result.Summaries = append(result.Summaries, model.SessionSummary{
			ID:              meta.ID,
			Path:            path,
			CWD:             meta.CWD,
			Originator:      meta.Originator,
			CLIVersion:      meta.CLIVersion,
			StartedAt:       meta.StartedAt,
			Summary:         summaryText,
			MessageCount:    count,
			DurationSeconds: duration,
		})

		return nil
	})
	if err != nil {
		return result, err
	}

	sort.Slice(result.Summaries, func(i, j int) bool {
		return result.Summaries[i].StartedAt.After(result.Summaries[j].StartedAt)
	})

	if opts.Limit > 0 && len(result.Summaries) > opts.Limit {
		result.Summaries = result.Summaries[:opts.Limit]
	}

	return result, nil
}

func truncate(s string, max int) string {
	if len(s) <= max {
		return s
	}
	runes := []rune(s)
	if len(runes) <= max {
		return s
	}
	return string(runes[:max]) + "…"
}

// FindSessionPath searches for a session file whose session_meta id matches id.
func FindSessionPath(root, id string) (string, error) {
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
		if meta.ID == id {
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
