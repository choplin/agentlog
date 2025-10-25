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
	"codexlog/internal/model"
	"codexlog/internal/parser"
	"codexlog/internal/store"

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
	flags.BoolVar(&noHeader, "no-header", false, "omit header row for tsv output")
	flags.IntVar(&summaryWidth, "summary-width", 160, "maximum characters included in the summary column")
	flags.StringVar(&sessionsDir, "sessions-dir", defaultSessionsDir(), "override the sessions directory")

	return cmd
}

func newViewCmd() *cobra.Command {
	var (
		role        string
		raw         bool
		wrap        int
		sessionsDir string
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
			if raw {
				return copyFile(out, path)
			}

			meta, err := parser.ReadSessionMeta(path)
			if err != nil {
				return err
			}

			fmt.Fprintf(out, "Session ID: %s\n", meta.ID)
			fmt.Fprintf(out, "Started At: %s\n", meta.StartedAt.Format(time.RFC3339))
			fmt.Fprintf(out, "CWD: %s\n", meta.CWD)
			fmt.Fprintf(out, "Originator: %s\n", meta.Originator)
			fmt.Fprintf(out, "CLI Version: %s\n", meta.CLIVersion)
			fmt.Fprintf(out, "File: %s\n\n", path)

			lowerRole := strings.ToLower(role)
			return parser.IterateEvents(path, lowerRole, func(event model.Event) error {
				if event.Kind == "session_meta" {
					return nil
				}
				line := format.RenderEvent(event, wrap)
				_, err := fmt.Fprintln(out, line)
				return err
			})
		},
	}

	flags := cmd.Flags()
	flags.StringVar(&role, "role", "", "filter messages by role (user, assistant, tool)")
	flags.BoolVar(&raw, "raw", false, "output raw JSONL without formatting")
	flags.IntVar(&wrap, "wrap", 0, "wrap message body at the given column width")
	flags.StringVar(&sessionsDir, "sessions-dir", defaultSessionsDir(), "override the sessions directory")

	return cmd
}

func newInfoCmd() *cobra.Command {
	var (
		formatFlag  string
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

			payload := struct {
				SessionID       string `json:"session_id"`
				JSONLPath       string `json:"jsonl_path"`
				StartedAt       string `json:"started_at"`
				CWD             string `json:"cwd"`
				Originator      string `json:"originator"`
				CLIVersion      string `json:"cli_version"`
				MessageCount    int    `json:"message_count"`
				DurationSeconds int    `json:"duration_seconds"`
				Summary         string `json:"summary"`
			}{
				SessionID:       meta.ID,
				JSONLPath:       path,
				StartedAt:       meta.StartedAt.Format(time.RFC3339),
				CWD:             meta.CWD,
				Originator:      meta.Originator,
				CLIVersion:      meta.CLIVersion,
				MessageCount:    count,
				DurationSeconds: duration,
				Summary:         summary,
			}

			switch strings.ToLower(formatFlag) {
			case "json":
				enc := json.NewEncoder(cmd.OutOrStdout())
				enc.SetIndent("", "  ")
				return enc.Encode(payload)
			case "text":
				out := cmd.OutOrStdout()
				fmt.Fprintf(out, "Session ID: %s\n", payload.SessionID)
				fmt.Fprintf(out, "JSONL Path: %s\n", payload.JSONLPath)
				fmt.Fprintf(out, "Started At: %s\n", payload.StartedAt)
				fmt.Fprintf(out, "CWD: %s\n", payload.CWD)
				fmt.Fprintf(out, "Originator: %s\n", payload.Originator)
				fmt.Fprintf(out, "CLI Version: %s\n", payload.CLIVersion)
				fmt.Fprintf(out, "Message Count: %d\n", payload.MessageCount)
				fmt.Fprintf(out, "Duration: %s\n", formatDuration(payload.DurationSeconds))
				fmt.Fprintf(out, "Summary: %s\n", payload.Summary)
				return nil
			default:
				return fmt.Errorf("unsupported format: %s", formatFlag)
			}
		},
	}

	flags := cmd.Flags()
	flags.StringVar(&formatFlag, "format", "json", "output format: json or text")
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

func copyFile(dst io.Writer, path string) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()

	_, err = io.Copy(dst, f)
	return err
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
