package format

import (
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"time"

	"codexlog/internal/model"
)

// WriteSummaries writes session summaries to w in the requested format.
func WriteSummaries(w io.Writer, items []model.SessionSummary, includeHeader bool, format string) error {
	switch format {
	case "tsv":
		return writeSummariesTSV(w, items, includeHeader)
	case "json":
		return writeSummariesJSON(w, items)
	default:
		return fmt.Errorf("unsupported format: %s", format)
	}
}

func writeSummariesTSV(w io.Writer, items []model.SessionSummary, includeHeader bool) error {
	if includeHeader {
		if _, err := fmt.Fprintln(w, "timestamp\tsession_id\tcwd\tsummary"); err != nil {
			return err
		}
	}

	for _, item := range items {
		line := fmt.Sprintf(
			"%s\t%s\t%s\t%s",
			item.StartedAt.Format(time.RFC3339),
			item.ID,
			item.CWD,
			escapeNewlines(item.Summary),
		)
		if _, err := fmt.Fprintln(w, line); err != nil {
			return err
		}
	}
	return nil
}

func writeSummariesJSON(w io.Writer, items []model.SessionSummary) error {
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	return enc.Encode(items)
}

func escapeNewlines(text string) string {
	return strings.ReplaceAll(text, "\n", "\\n")
}
