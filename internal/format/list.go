package format

import (
	"encoding/json"
	"fmt"
	"io"
	"strings"
	"time"

	"github.com/jedib0t/go-pretty/v6/table"
	"github.com/jedib0t/go-pretty/v6/text"

	"codexlog/internal/model"
)

// WriteSummaries writes session summaries to w in the requested format.
func WriteSummaries(w io.Writer, items []model.SessionSummary, includeHeader bool, format string) error {
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

func writeSummariesPlain(w io.Writer, items []model.SessionSummary, includeHeader bool) error {
	if includeHeader {
		if _, err := fmt.Fprintln(w, "timestamp\tsession_id\tcwd\tmessage_count\tsummary"); err != nil {
			return err
		}
	}

	for _, item := range items {
		line := fmt.Sprintf(
			"%s\t%s\t%s\t%d\t%s",
			item.StartedAt.Format(time.RFC3339),
			item.ID,
			item.CWD,
			item.MessageCount,
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

func writeSummariesJSONL(w io.Writer, items []model.SessionSummary) error {
	enc := json.NewEncoder(w)
	for _, item := range items {
		if err := enc.Encode(item); err != nil {
			return err
		}
	}
	return nil
}

func escapeNewlines(text string) string {
	return strings.ReplaceAll(text, "\n", "\\n")
}

func writeSummariesTable(w io.Writer, items []model.SessionSummary, includeHeader bool) error {
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
		{Number: 4, Align: text.AlignRight, AlignHeader: text.AlignCenter},
		{Number: 5, Align: text.AlignLeft, AlignHeader: text.AlignCenter, WidthMax: 80},
	})

	if includeHeader {
		tw.AppendHeader(table.Row{"Timestamp", "Session ID", "CWD", "Messages", "Summary"})
	}

	for _, item := range items {
		tw.AppendRow(table.Row{
			item.StartedAt.Format(time.RFC3339),
			item.ID,
			item.CWD,
			item.MessageCount,
			escapeNewlines(item.Summary),
		})
	}

	if len(items) == 0 {
		tw.AppendRow(table.Row{"-", "(no sessions)", "-", 0, "-"})
	}

	_ = tw.Render()
	return nil
}
