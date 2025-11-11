// Package format provides formatting and rendering functions for session data.
package format

import (
	"agentlog/internal/model"
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"time"

	"github.com/jedib0t/go-pretty/v6/table"
	"github.com/jedib0t/go-pretty/v6/text"
)

// WriteSummaries writes session summaries to w in the requested format.
func WriteSummaries(w io.Writer, items []model.SessionSummaryProvider, includeHeader bool, format string) error {
	format = strings.ToLower(format)
	switch format {
	case "", "table":
		return writeSummariesTable(w, items, includeHeader)
	case "plain":
		return writeSummariesPlain(w, items, includeHeader)
	case "json":
		return writeSummariesJSON(w, items)
	case "jsonl":
		return writeSummariesJSONL(w, items)
	default:
		return fmt.Errorf("unsupported format: %s", format)
	}
}

func writeSummariesPlain(w io.Writer, items []model.SessionSummaryProvider, includeHeader bool) error {
	if includeHeader {
		if _, err := fmt.Fprintln(w, "timestamp\tsession_id\tcwd\tduration\tmessage_count\tsummary"); err != nil {
			return err
		}
	}

	for _, item := range items {
		line := fmt.Sprintf(
			"%s\t%s\t%s\t%s\t%d\t%s",
			item.GetStartedAt().Format(time.RFC3339),
			item.GetID(),
			item.GetCWD(),
			formatDuration(item.GetDurationSeconds()),
			item.GetMessageCount(),
			escapeNewlines(item.GetSummary()),
		)
		if _, err := fmt.Fprintln(w, line); err != nil {
			return err
		}
	}
	return nil
}

func writeSummariesJSON(w io.Writer, items []model.SessionSummaryProvider) error {
	// Convert to a serializable format
	output := make([]map[string]interface{}, len(items))
	for i, item := range items {
		output[i] = map[string]interface{}{
			"id":               item.GetID(),
			"path":             item.GetPath(),
			"cwd":              item.GetCWD(),
			"started_at":       item.GetStartedAt(),
			"summary":          item.GetSummary(),
			"message_count":    item.GetMessageCount(),
			"duration_seconds": item.GetDurationSeconds(),
		}
	}
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	return enc.Encode(output)
}

func writeSummariesJSONL(w io.Writer, items []model.SessionSummaryProvider) error {
	enc := json.NewEncoder(w)
	for _, item := range items {
		output := map[string]interface{}{
			"id":               item.GetID(),
			"path":             item.GetPath(),
			"cwd":              item.GetCWD(),
			"started_at":       item.GetStartedAt(),
			"summary":          item.GetSummary(),
			"message_count":    item.GetMessageCount(),
			"duration_seconds": item.GetDurationSeconds(),
		}
		if err := enc.Encode(output); err != nil {
			return err
		}
	}
	return nil
}

func escapeNewlines(text string) string {
	return strings.ReplaceAll(text, "\n", "\\n")
}

func writeSummariesTable(w io.Writer, items []model.SessionSummaryProvider, includeHeader bool) error {
	tw := table.NewWriter()
	tw.SetOutputMirror(w)
	tw.SetStyle(table.StyleRounded)
	tw.Style().Options.SeparateRows = true
	tw.Style().Options.SeparateHeader = true
	tw.Style().Options.DrawBorder = true

	tw.SetColumnConfigs([]table.ColumnConfig{
		{Number: 1, Align: text.AlignLeft, AlignHeader: text.AlignCenter},
		{Number: 2, Align: text.AlignLeft, AlignHeader: text.AlignCenter},
		{Number: 3, Align: text.AlignLeft, AlignHeader: text.AlignCenter},
		{Number: 4, Align: text.AlignCenter, AlignHeader: text.AlignCenter},
		{Number: 5, Align: text.AlignRight, AlignHeader: text.AlignCenter},
		{Number: 6, Align: text.AlignLeft, AlignHeader: text.AlignCenter, WidthMax: 80},
	})

	if includeHeader {
		tw.AppendHeader(table.Row{"Timestamp", "Session ID", "CWD", "Duration", "Messages", "Summary"})
	}

	for _, item := range items {
		tw.AppendRow(table.Row{
			item.GetStartedAt().Format(time.RFC3339),
			item.GetID(),
			item.GetCWD(),
			formatDuration(item.GetDurationSeconds()),
			item.GetMessageCount(),
			escapeNewlines(item.GetSummary()),
		})
	}

	if len(items) == 0 {
		tw.AppendRow(table.Row{"-", "(no sessions)", "-", "00:00:00", 0, "-"})
	}

	_ = tw.Render()
	return nil
}

func formatDuration(seconds int) string {
	if seconds <= 0 {
		return "00:00:00"
	}
	h := seconds / 3600
	m := (seconds % 3600) / 60
	s := seconds % 60
	return fmt.Sprintf("%02d:%02d:%02d", h, m, s)
}
