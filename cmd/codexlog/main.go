package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"codexlog/internal/format"
	"codexlog/internal/parser"
	"codexlog/internal/store"
	"codexlog/internal/view"

	"github.com/spf13/cobra"
)

var rootCmd = &cobra.Command{
	Use:   "codexlog",
	Short: "Browse Codex CLI session logs",
}

func init() {
	rootCmd.AddCommand(newListCmd())
	rootCmd.AddCommand(newViewCmd())
	rootCmd.AddCommand(newInfoCmd())
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintf(os.Stderr, "codexlog: %v\n", err)
		os.Exit(1)
	}
}

func newListCmd() *cobra.Command {
	var (
		cwd          string
		all          bool
		afterStr     string
		beforeStr    string
		limit        int
		formatFlag   string
		noHeader     bool
		summaryWidth int
		sessionsDir  string
	)

	cmd := &cobra.Command{
		Use:   "list",
		Short: "List session metadata in reverse chronological order",
		RunE: func(cmd *cobra.Command, args []string) error {
			if all && cwd != "" {
				return errors.New("--cwd cannot be used with --all")
			}

			var after, before *time.Time
			if afterStr != "" {
				t, err := time.Parse(time.RFC3339, afterStr)
				if err != nil {
					return fmt.Errorf("invalid --after value: %w", err)
				}
				after = &t
			}
			if beforeStr != "" {
				t, err := time.Parse(time.RFC3339, beforeStr)
				if err != nil {
					return fmt.Errorf("invalid --before value: %w", err)
				}
				before = &t
			}

			opts := store.ListOptions{
				Root:       sessionsDir,
				After:      after,
				Before:     before,
				Limit:      limit,
				MaxSummary: summaryWidth,
			}

			if !all {
				if cwd != "" {
					opts.CWD = cwd
				} else {
					wd, err := os.Getwd()
					if err != nil {
						return fmt.Errorf("determine current directory: %w", err)
					}
					opts.CWD = wd
				}
				opts.ExactCWD = true
			} else if cwd != "" {
				opts.CWD = cwd
			}

			result, err := store.ListSessions(opts)
			if err != nil {
				return err
			}

			errs := cmd.ErrOrStderr()
			for _, warn := range result.Warnings {
				fmt.Fprintf(errs, "warning: %v\n", warn)
			}

			includeHeader := !noHeader
			if err := format.WriteSummaries(cmd.OutOrStdout(), result.Summaries, includeHeader, strings.ToLower(formatFlag)); err != nil {
				return err
			}

			return nil
		},
	}

	flags := cmd.Flags()
	flags.StringVar(&cwd, "cwd", "", "filter sessions whose cwd equals the provided path")
	flags.BoolVar(&all, "all", false, "include sessions from all directories")
	flags.StringVar(&afterStr, "after", "", "include sessions starting on/after the given RFC3339 timestamp")
	flags.StringVar(&beforeStr, "before", "", "include sessions starting on/before the given RFC3339 timestamp")
	flags.IntVar(&limit, "limit", 0, "limit number of sessions returned (0 means no limit)")
	flags.StringVar(&formatFlag, "format", "table", "output format: table, plain, json, or jsonl")
	flags.BoolVar(&noHeader, "no-header", false, "omit header row for plain output")
	flags.IntVar(&summaryWidth, "summary-width", 160, "maximum characters included in the summary column")
	flags.StringVar(&sessionsDir, "sessions-dir", defaultSessionsDir(), "override the sessions directory")

	return cmd
}

func newViewCmd() *cobra.Command {
	var (
		entryTypeArg   string
		payloadTypeArg string
		payloadRoleArg string
		allFilter      bool
		raw            bool
		wrap           int
		maxEvents      int
		sessionsDir    string
		formatFlag     string
		forceColor     bool
		forceNoColor   bool
	)

	cmd := &cobra.Command{
		Use:   "view <session-id-or-path>",
		Short: "Render a session transcript",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			path, err := resolveSessionPath(args[0], sessionsDir)
			if err != nil {
				return err
			}

			out := cmd.OutOrStdout()
			if forceColor && forceNoColor {
				return errors.New("--color and --no-color cannot be used together")
			}

			// Check for exclusive flag usage
			if allFilter && (entryTypeArg != "" || payloadTypeArg != "" || payloadRoleArg != "") {
				return errors.New("--all cannot be used with -E, -T, or -R flags")
			}

			outFile, _ := out.(*os.File)
			return view.Run(view.Options{
				Path:           path,
				Format:         formatFlag,
				Wrap:           wrap,
				MaxEvents:      maxEvents,
				EntryTypeArg:   entryTypeArg,
				PayloadTypeArg: payloadTypeArg,
				PayloadRoleArg: payloadRoleArg,
				AllFilter:      allFilter,
				ForceColor:     forceColor,
				ForceNoColor:   forceNoColor,
				RawFile:        raw,
				Out:            out,
				OutFile:        outFile,
			})
		},
	}

	flags := cmd.Flags()
	flags.StringVarP(&entryTypeArg, "entry-type", "E", "", "comma-separated entry types to include (default: response_item)")
	flags.StringVarP(&payloadTypeArg, "payload-type", "T", "", "comma-separated payload types to include (default: message)")
	flags.StringVarP(&payloadRoleArg, "payload-role", "R", "", "comma-separated payload roles to include (default: user,assistant; use 'all' for every role)")
	flags.BoolVar(&allFilter, "all", false, "show all entries including session_meta (overrides -E, -T, and -R)")
	flags.BoolVar(&raw, "raw", false, "output raw JSONL without formatting")
	flags.IntVar(&wrap, "wrap", 0, "wrap message body at the given column width")
	flags.IntVar(&maxEvents, "max", 0, "show only the most recent N events (0 means no limit)")
	flags.StringVar(&sessionsDir, "sessions-dir", defaultSessionsDir(), "override the sessions directory")
	flags.StringVar(&formatFlag, "format", "text", "output format: text, chat, or raw")
	flags.BoolVar(&forceColor, "color", false, "force-enable ANSI colors even when stdout is not a TTY")
	flags.BoolVar(&forceNoColor, "no-color", false, "disable ANSI colors regardless of terminal detection")

	return cmd
}

type infoPayload struct {
	SessionID       string `json:"session_id"`
	JSONLPath       string `json:"jsonl_path"`
	StartedAt       string `json:"started_at"`
	CWD             string `json:"cwd"`
	Originator      string `json:"originator"`
	CLIVersion      string `json:"cli_version"`
	MessageCount    int    `json:"message_count"`
	DurationSeconds int    `json:"duration_seconds"`
	DurationDisplay string `json:"duration_display"`
	Summary         string `json:"summary"`
}

func newInfoCmd() *cobra.Command {
	var (
		formatFlag  string
		summaryMode string
		sessionsDir string
	)

	cmd := &cobra.Command{
		Use:   "info <session-id-or-path>",
		Short: "Show session metadata and file details",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			path, err := resolveSessionPath(args[0], sessionsDir)
			if err != nil {
				return err
			}

			meta, err := parser.ReadSessionMeta(path)
			if err != nil {
				return err
			}

			summary, count, lastTimestamp, err := parser.FirstUserSummary(path)
			if err != nil {
				return err
			}

			if lastTimestamp.IsZero() || lastTimestamp.Before(meta.StartedAt) {
				lastTimestamp = meta.StartedAt
			}
			duration := durationSeconds(meta.StartedAt, lastTimestamp)

			summaryMode = strings.ToLower(summaryMode)
			switch summaryMode {
			case "", "clip":
			case "full":
			default:
				return fmt.Errorf("invalid --summary value: %s", summaryMode)
			}

			summarySnippet := collapseWhitespace(summary)
			if summaryMode != "full" {
				summarySnippet = clipSummary(summarySnippet, 160)
			}

			payload := infoPayload{
				SessionID:       meta.ID,
				JSONLPath:       path,
				StartedAt:       meta.StartedAt.Format(time.RFC3339),
				CWD:             meta.CWD,
				Originator:      meta.Originator,
				CLIVersion:      meta.CLIVersion,
				MessageCount:    count,
				DurationSeconds: duration,
				DurationDisplay: formatDuration(duration),
				Summary:         summary,
			}

			switch strings.ToLower(formatFlag) {
			case "json":
				enc := json.NewEncoder(cmd.OutOrStdout())
				enc.SetIndent("", "  ")
				return enc.Encode(payload)
			case "text":
				out := cmd.OutOrStdout()
				renderInfoText(out, payload, summarySnippet)
				return nil
			default:
				return fmt.Errorf("unsupported format: %s", formatFlag)
			}
		},
	}

	flags := cmd.Flags()
	flags.StringVar(&formatFlag, "format", "text", "output format: text or json")
	flags.StringVar(&summaryMode, "summary", "clip", "summary display: clip or full")
	flags.StringVar(&sessionsDir, "sessions-dir", defaultSessionsDir(), "override the sessions directory")

	return cmd
}

func resolveSessionPath(arg, root string) (string, error) {
	if arg == "" {
		return "", errors.New("session identifier is empty")
	}

	if info, err := os.Stat(arg); err == nil && !info.IsDir() {
		return arg, nil
	}

	candidate := filepath.Join(root, arg)
	if info, err := os.Stat(candidate); err == nil && !info.IsDir() {
		return candidate, nil
	}

	return store.FindSessionPath(root, arg)
}

func defaultSessionsDir() string {
	if dir := os.Getenv("CODEXLOG_SESSIONS_DIR"); dir != "" {
		return dir
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return ""
	}
	return filepath.Join(home, ".codex", "sessions")
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

func formatDuration(seconds int) string {
	if seconds <= 0 {
		return "00:00:00"
	}
	h := seconds / 3600
	m := (seconds % 3600) / 60
	s := seconds % 60
	return fmt.Sprintf("%02d:%02d:%02d", h, m, s)
}

func renderInfoText(out io.Writer, payload infoPayload, summarySnippet string) {
	const labelWidth = 14
	writeKV(out, labelWidth, "Session ID", payload.SessionID)
	writeKV(out, labelWidth, "Started At", payload.StartedAt)
	writeKV(out, labelWidth, "Duration", payload.DurationDisplay)
	writeKV(out, labelWidth, "CWD", payload.CWD)
	writeKV(out, labelWidth, "Originator", payload.Originator)
	writeKV(out, labelWidth, "CLI Version", payload.CLIVersion)
	writeKV(out, labelWidth, "Message Count", fmt.Sprintf("%d", payload.MessageCount))
	writeKV(out, labelWidth, "JSONL Path", payload.JSONLPath)
	writeKV(out, labelWidth, "Summary", summarySnippet)

}

func writeKV(out io.Writer, width int, label string, value string) {
	fmt.Fprintf(out, "%-*s: %s\n", width, label, value)
}

func collapseWhitespace(text string) string {
	return strings.Join(strings.Fields(strings.TrimSpace(text)), " ")
}

func clipSummary(text string, max int) string {
	if max <= 0 {
		return ""
	}
	runes := []rune(text)
	if len(runes) <= max {
		return text
	}
	if max == 1 {
		return "…"
	}
	return string(runes[:max-1]) + "…"
}
