package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"codexlog/internal/format"
	"codexlog/internal/model"
	"codexlog/internal/parser"
	"codexlog/internal/store"

	"github.com/mattn/go-isatty"
	"github.com/spf13/cobra"
	"golang.org/x/term"
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

			if raw {
				return copyFile(out, path)
			}

			filters, err := buildViewFilters(entryTypeArg, payloadTypeArg, payloadRoleArg)
			if err != nil {
				return err
			}

			formatMode := strings.ToLower(formatFlag)

			if _, err := parser.ReadSessionMeta(path); err != nil {
				return err
			}

			processEvents := func(fn func(model.Event) error) error {
				return parser.IterateEvents(path, func(event model.Event) error {
					if event.Kind == model.EntryTypeSessionMeta {
						return nil
					}
					if !eventMatchesFilters(event, filters) {
						return nil
					}
					return fn(event)
				})
			}

			switch formatMode {
			case "", "text":
				useColor := resolveColorChoice(out, forceColor, forceNoColor)
				if maxEvents == 0 {
					count := 0
					return processEvents(func(event model.Event) error {
						if count > 0 {
							fmt.Fprintln(out)
						}
						printEvent(out, event, count+1, wrap, useColor)
						count++
						return nil
					})
				}
				ring := newEventRing(maxEvents)
				if err := processEvents(func(event model.Event) error {
					ring.push(event)
					return nil
				}); err != nil {
					return err
				}
				for idx, event := range ring.slice() {
					if idx > 0 {
						fmt.Fprintln(out)
					}
					printEvent(out, event, idx+1, wrap, useColor)
				}
				return nil

			case "raw":
				if maxEvents == 0 {
					return processEvents(func(event model.Event) error {
						_, err := fmt.Fprintln(out, event.Raw)
						return err
					})
				}
				ring := newEventRing(maxEvents)
				if err := processEvents(func(event model.Event) error {
					ring.push(event)
					return nil
				}); err != nil {
					return err
				}
				for _, event := range ring.slice() {
					fmt.Fprintln(out, event.Raw)
				}
				return nil

			case "chat":
				colorEnabled := resolveColorChoice(out, forceColor, forceNoColor)
				outFile, outIsFile := out.(*os.File)
				width := determineWidth(outFile, wrap)

				var events []model.Event
				if maxEvents > 0 {
					ring := newEventRing(maxEvents)
					if err := processEvents(func(event model.Event) error {
						ring.push(event)
						return nil
					}); err != nil {
						return err
					}
					events = ring.slice()
				} else {
					collected := make([]model.Event, 0)
					if err := processEvents(func(event model.Event) error {
						collected = append(collected, event)
						return nil
					}); err != nil {
						return err
					}
					events = collected
				}

				if len(events) == 0 {
					return nil
				}

				lines := renderChatTranscript(events, width, colorEnabled)
				if len(lines) == 0 {
					return nil
				}
				if outIsFile && isatty.IsTerminal(outFile.Fd()) {
					return pipeThroughPager(lines, colorEnabled)
				}
				return writeLines(out, lines)

			default:
				return fmt.Errorf("unsupported format: %s", formatFlag)
			}
		},
	}

	flags := cmd.Flags()
	flags.StringVarP(&entryTypeArg, "entry-type", "E", "", "comma-separated entry types to include (default: all)")
	flags.StringVarP(&payloadTypeArg, "payload-type", "T", "", "comma-separated payload types to include (default: all)")
	flags.StringVarP(&payloadRoleArg, "payload-role", "R", "", "comma-separated payload roles to include (default: user,assistant; use 'all' for every role)")
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

type viewFilters struct {
	entryTypes   map[model.EntryType]struct{}
	payloadTypes map[model.PayloadType]struct{}
	payloadRoles map[model.PayloadRole]struct{}
}

func buildViewFilters(entryArg, payloadTypeArg, payloadRoleArg string) (viewFilters, error) {
	var filters viewFilters

	entryFilter, entryProvided, err := parseEntryTypeArg(entryArg)
	if err != nil {
		return filters, err
	}
	payloadTypeFilter, typeProvided, err := parsePayloadTypeArg(payloadTypeArg)
	if err != nil {
		return filters, err
	}
	payloadRoleFilter, provided, err := parsePayloadRoleArg(payloadRoleArg)
	if err != nil {
		return filters, err
	}

	if entryProvided {
		filters.entryTypes = entryFilter
	} else {
		filters.entryTypes = map[model.EntryType]struct{}{
			model.EntryTypeResponseItem: {},
		}
	}
	if typeProvided {
		filters.payloadTypes = payloadTypeFilter
	} else {
		filters.payloadTypes = map[model.PayloadType]struct{}{
			model.PayloadTypeMessage: {},
		}
	}
	if provided {
		filters.payloadRoles = payloadRoleFilter
	} else {
		filters.payloadRoles = map[model.PayloadRole]struct{}{
			model.PayloadRoleUser:      {},
			model.PayloadRoleAssistant: {},
		}
	}

	return filters, nil
}

func parseEntryTypeArg(arg string) (map[model.EntryType]struct{}, bool, error) {
	values := parseCSV(arg)
	if len(values) == 0 {
		return nil, false, nil
	}
	if len(values) == 1 && values[0] == "all" {
		return nil, true, nil
	}

	lookup := map[string]model.EntryType{
		"session_meta":  model.EntryTypeSessionMeta,
		"response_item": model.EntryTypeResponseItem,
	}

	set := make(map[model.EntryType]struct{}, len(values))
	for _, token := range values {
		entryType, ok := lookup[token]
		if !ok {
			return nil, true, fmt.Errorf("unknown entry type %q", token)
		}
		set[entryType] = struct{}{}
	}
	return set, true, nil
}

func parsePayloadTypeArg(arg string) (map[model.PayloadType]struct{}, bool, error) {
	values := parseCSV(arg)
	if len(values) == 0 {
		return nil, false, nil
	}
	if len(values) == 1 && values[0] == "all" {
		return nil, true, nil
	}

	lookup := map[string]model.PayloadType{
		"message":      model.PayloadTypeMessage,
		"event_msg":    model.PayloadTypeEventMsg,
		"turn_context": model.PayloadTypeTurnContext,
	}

	set := make(map[model.PayloadType]struct{}, len(values))
	for _, token := range values {
		payloadType, ok := lookup[token]
		if !ok {
			return nil, true, fmt.Errorf("unknown payload type %q", token)
		}
		set[payloadType] = struct{}{}
	}
	return set, true, nil
}

func parsePayloadRoleArg(arg string) (map[model.PayloadRole]struct{}, bool, error) {
	values := parseCSV(arg)
	if len(values) == 0 {
		return nil, false, nil
	}
	if len(values) == 1 && values[0] == "all" {
		return nil, true, nil
	}

	lookup := map[string]model.PayloadRole{
		"user":      model.PayloadRoleUser,
		"assistant": model.PayloadRoleAssistant,
		"tool":      model.PayloadRoleTool,
		"system":    model.PayloadRoleSystem,
	}

	set := make(map[model.PayloadRole]struct{}, len(values))
	for _, token := range values {
		role, ok := lookup[token]
		if !ok {
			return nil, true, fmt.Errorf("unknown payload role %q", token)
		}
		set[role] = struct{}{}
	}
	return set, true, nil
}

func parseCSV(arg string) []string {
	if strings.TrimSpace(arg) == "" {
		return nil
	}
	parts := strings.Split(arg, ",")
	output := make([]string, 0, len(parts))
	for _, part := range parts {
		token := strings.TrimSpace(strings.ToLower(part))
		if token != "" {
			output = append(output, token)
		}
	}
	return output
}

func eventMatchesFilters(event model.Event, filters viewFilters) bool {
	if filters.entryTypes != nil {
		if _, ok := filters.entryTypes[event.Kind]; !ok {
			return false
		}
	}

	if event.Kind == model.EntryTypeResponseItem {
		if filters.payloadTypes != nil {
			if _, ok := filters.payloadTypes[event.MessageType]; !ok {
				return false
			}
		}
		if filters.payloadRoles != nil {
			if _, ok := filters.payloadRoles[event.Role]; !ok {
				return false
			}
		}
	}

	return true
}

type eventRing struct {
	data   []model.Event
	start  int
	length int
}

func newEventRing(capacity int) *eventRing {
	if capacity <= 0 {
		return &eventRing{}
	}
	return &eventRing{data: make([]model.Event, capacity)}
}

func (r *eventRing) push(event model.Event) {
	if len(r.data) == 0 {
		return
	}
	idx := (r.start + r.length) % len(r.data)
	r.data[idx] = event
	if r.length < len(r.data) {
		r.length++
		return
	}
	r.start = (r.start + 1) % len(r.data)
}

func (r *eventRing) slice() []model.Event {
	if r.length == 0 {
		return nil
	}
	result := make([]model.Event, r.length)
	for i := 0; i < r.length; i++ {
		result[i] = r.data[(r.start+i)%len(r.data)]
	}
	return result
}

func determineWidth(out *os.File, wrap int) int {
	if wrap > 0 {
		return wrap
	}
	if out != nil {
		if w, _, err := term.GetSize(int(out.Fd())); err == nil && w > 0 {
			return w
		}
	}
	if colsStr := os.Getenv("COLUMNS"); colsStr != "" {
		if v, err := strconv.Atoi(colsStr); err == nil && v > 0 {
			return v
		}
	}
	return 80
}

func pipeThroughPager(lines []string, colorEnabled bool) error {
	text := strings.Join(lines, "\n")
	if !strings.HasSuffix(text, "\n") {
		text += "\n"
	}

	pagerCmd := os.Getenv("PAGER")
	var cmd *exec.Cmd
	if pagerCmd == "" {
		args := []string{"less"}
		if colorEnabled {
			args = append(args, "-R")
		}
		cmd = exec.Command(args[0], args[1:]...) // #nosec G204
	} else {
		cmd = exec.Command("sh", "-c", pagerCmd) // #nosec G204
	}

	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	stdin, err := cmd.StdinPipe()
	if err != nil {
		return fmt.Errorf("create pager pipe: %w", err)
	}
	go func() {
		defer stdin.Close()
		io.WriteString(stdin, text) //nolint:errcheck
	}()

	if err := cmd.Run(); err != nil {
		return fmt.Errorf("run pager: %w", err)
	}

	return nil
}

func writeLines(out io.Writer, lines []string) error {
	for _, line := range lines {
		if _, err := fmt.Fprintln(out, line); err != nil {
			return err
		}
	}
	return nil
}

func printEvent(out io.Writer, event model.Event, index int, wrap int, useColor bool) {
	roleLabel := string(event.Role)
	if roleLabel == "" {
		roleLabel = string(event.Kind)
	}
	if roleLabel == "" {
		roleLabel = "event"
	}
	roleLabel = strings.ToLower(roleLabel)

	ts := "-"
	if !event.Timestamp.IsZero() {
		ts = event.Timestamp.Format(time.RFC3339)
	}
	headerPlain := fmt.Sprintf("[#%03d] %s | %s", index, roleLabel, ts)

	indexText := fmt.Sprintf("#%03d", index)
	roleText := roleLabel
	tsText := ts
	separator := "|"

	if useColor {
		indexText = colorize(true, ansiBoldWhite, indexText)
		roleText = colorize(true, roleColor(roleLabel), roleText)
		tsText = colorize(true, ansiTimestamp, tsText)
		separator = colorize(true, ansiSeparator, "|")
	}

	header := fmt.Sprintf("[%s] %s %s %s", indexText, roleText, separator, tsText)
	fmt.Fprintln(out, header)
	fmt.Fprintln(out, strings.Repeat("-", len(headerPlain)))

	lines := format.RenderEventLines(event, wrap)
	if len(lines) == 0 {
		prefix := "|"
		if useColor {
			prefix = colorize(true, ansiSeparator, "|")
		}
		fmt.Fprintf(out, "%s %s\n", prefix, "(no content)")
		return
	}
	linePrefix := "| "
	emptyPrefix := "|"
	if useColor {
		separatorColor := colorize(true, ansiSeparator, "|")
		linePrefix = separatorColor + " "
		emptyPrefix = separatorColor
	}
	for _, line := range lines {
		if line == "" {
			fmt.Fprintln(out, emptyPrefix)
			continue
		}
		fmt.Fprintf(out, "%s%s\n", linePrefix, line)
	}
}

const (
	ansiReset     = "\x1b[0m"
	ansiBoldWhite = "\x1b[1;97m"
	ansiTimestamp = "\x1b[38;5;245m"
	ansiSeparator = "\x1b[38;5;240m"
	ansiAssistant = "\x1b[38;5;44m"
	ansiUser      = "\x1b[38;5;220m"
	ansiTool      = "\x1b[38;5;207m"
)

func colorize(enabled bool, code string, text string) string {
	if !enabled {
		return text
	}
	return code + text + ansiReset
}

func roleColor(role string) string {
	switch model.PayloadRole(role) {
	case model.PayloadRoleAssistant:
		return ansiAssistant
	case model.PayloadRoleUser:
		return ansiUser
	case model.PayloadRoleTool, model.PayloadRoleSystem:
		return ansiTool
	default:
		return ansiSeparator
	}
}

func resolveColorChoice(out io.Writer, forceColor, forceNoColor bool) bool {
	if forceColor {
		return true
	}
	if forceNoColor {
		return false
	}
	return shouldUseColorAuto(out)
}

func shouldUseColorAuto(out io.Writer) bool {
	if os.Getenv("NO_COLOR") != "" {
		return false
	}
	file, ok := out.(*os.File)
	if !ok {
		return false
	}
	fd := file.Fd()
	return isatty.IsTerminal(fd) || isatty.IsCygwinTerminal(fd)
}
