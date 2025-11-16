// Package main provides the agentlog CLI for browsing AI agent conversation logs.
package main

import (
	// Import both agent packages to trigger init() registration
	_ "agentlog/internal/claude"
	_ "agentlog/internal/codex"
	"agentlog/internal/format"
	"agentlog/internal/model"
	"agentlog/internal/store"
	"agentlog/internal/view"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/spf13/cobra"
)

var version = "dev"

var agentType string

var rootCmd = &cobra.Command{
	Use:     "agentlog",
	Short:   "Browse, search, and analyze AI agent conversation logs",
	Version: version,
}

func init() {
	rootCmd.PersistentFlags().StringVar(&agentType, "agent", "",
		"Agent type: 'codex' or 'claude' (env: AGENTLOG_AGENT, default: claude)")

	rootCmd.AddCommand(newListCmd())
	rootCmd.AddCommand(newViewCmd())
	rootCmd.AddCommand(newInfoCmd())
}

// getAgentType returns the agent type from flag, environment variable, or default.
func getAgentType() model.AgentType {
	if agentType != "" {
		return model.AgentType(agentType)
	}
	if env := os.Getenv("AGENTLOG_AGENT"); env != "" {
		return model.AgentType(env)
	}
	return model.AgentClaude
}

// defaultSessionsDir returns the default sessions directory for the given agent type.
func defaultSessionsDir(agentType model.AgentType) string {
	if dir := os.Getenv("AGENTLOG_SESSIONS_DIR"); dir != "" {
		return dir
	}

	home, _ := os.UserHomeDir()
	switch agentType {
	case model.AgentCodex:
		return filepath.Join(home, ".codex", "sessions")
	case model.AgentClaude:
		return filepath.Join(home, ".claude", "projects")
	default:
		return filepath.Join(home, ".claude", "projects")
	}
}

func main() {
	if err := rootCmd.Execute(); err != nil {
		fmt.Fprintf(os.Stderr, "agentlog: %v\n", err)
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
		RunE: func(cmd *cobra.Command, _ []string) error {
			if all && cwd != "" {
				return errors.New("--cwd cannot be used with --all")
			}

			// Get agent type and create parser
			agent := getAgentType()
			parser, err := model.NewParser(agent)
			if err != nil {
				return fmt.Errorf("create parser: %w", err)
			}

			// Use default sessions dir if not provided
			if sessionsDir == "" {
				sessionsDir = defaultSessionsDir(agent)
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

			result, err := store.ListSessions(parser, opts)
			if err != nil {
				return err
			}

			errs := cmd.ErrOrStderr()
			for _, warn := range result.Warnings {
				fmt.Fprintf(errs, "warning: %v\n", warn) //nolint:errcheck
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
	flags.StringVar(&sessionsDir, "sessions-dir", "", "override the sessions directory (default: agent-specific)")

	return cmd
}

func newViewCmd() *cobra.Command {
	var (
		level        string
		raw          bool
		wrap         int
		maxEvents    int
		sessionsDir  string
		formatFlag   string
		forceColor   bool
		forceNoColor bool
	)

	cmd := &cobra.Command{
		Use:   "view <session-id-or-path>",
		Short: "Render a session transcript",
		Args:  cobra.ExactArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			// Get agent type and create parser
			agent := getAgentType()
			parser, err := model.NewParser(agent)
			if err != nil {
				return fmt.Errorf("create parser: %w", err)
			}

			// Use default sessions dir if not provided
			if sessionsDir == "" {
				sessionsDir = defaultSessionsDir(agent)
			}

			path, err := resolveSessionPath(parser, args[0], sessionsDir)
			if err != nil {
				return err
			}

			out := cmd.OutOrStdout()
			if forceColor && forceNoColor {
				return errors.New("--color and --no-color cannot be used together")
			}

			outFile, _ := out.(*os.File)
			return view.Run(parser, view.Options{
				Path:         path,
				Format:       formatFlag,
				Wrap:         wrap,
				MaxEvents:    maxEvents,
				Level:        level,
				ForceColor:   forceColor,
				ForceNoColor: forceNoColor,
				RawFile:      raw,
				Out:          out,
				OutFile:      outFile,
			})
		},
	}

	flags := cmd.Flags()
	flags.StringVar(&level, "level", "conversation", "verbosity level: conversation (default) or all")
	flags.BoolVar(&raw, "raw", false, "output raw JSONL without formatting")
	flags.IntVar(&wrap, "wrap", 0, "wrap message body at the given column width")
	flags.IntVar(&maxEvents, "max", 0, "show only the most recent N events (0 means no limit)")
	flags.StringVar(&sessionsDir, "sessions-dir", "", "override the sessions directory (default: agent-specific)")
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
			// Get agent type and create parser
			agent := getAgentType()
			parser, err := model.NewParser(agent)
			if err != nil {
				return fmt.Errorf("create parser: %w", err)
			}

			// Use default sessions dir if not provided
			if sessionsDir == "" {
				sessionsDir = defaultSessionsDir(agent)
			}

			path, err := resolveSessionPath(parser, args[0], sessionsDir)
			if err != nil {
				return err
			}

			meta, err := parser.ReadSessionMeta(path)
			if err != nil {
				return err
			}

			summary, err := parser.FirstUserSummary(path)
			if err != nil {
				return err
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
				return err
			}

			if lastTimestamp.IsZero() || lastTimestamp.Before(meta.GetStartedAt()) {
				lastTimestamp = meta.GetStartedAt()
			}
			duration := durationSeconds(meta.GetStartedAt(), lastTimestamp)

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
				SessionID:       meta.GetID(),
				JSONLPath:       path,
				StartedAt:       meta.GetStartedAt().Format(time.RFC3339),
				CWD:             meta.GetCWD(),
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
	flags.StringVar(&sessionsDir, "sessions-dir", "", "override the sessions directory (default: agent-specific)")

	return cmd
}

func resolveSessionPath(parser model.Parser, arg, root string) (string, error) {
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

	return store.FindSessionPath(parser, root, arg)
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
	fmt.Fprintf(out, "%-*s: %s\n", width, label, value) //nolint:errcheck
}

func collapseWhitespace(text string) string {
	return strings.Join(strings.Fields(strings.TrimSpace(text)), " ")
}

func clipSummary(text string, maxLen int) string {
	if maxLen <= 0 {
		return ""
	}
	runes := []rune(text)
	if len(runes) <= maxLen {
		return text
	}
	if maxLen == 1 {
		return "…"
	}
	return string(runes[:maxLen-1]) + "…"
}
