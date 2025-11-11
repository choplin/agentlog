package view

import (
	"agentlog/internal/format"
	"agentlog/internal/model"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"

	"github.com/mattn/go-isatty"
	"golang.org/x/term"
)

// Options defines the configurable parameters for rendering a view.
type Options struct {
	Path            string
	Format          string
	Wrap            int
	MaxEvents       int
	EntryTypeArg    string
	ResponseTypeArg string
	EventMsgTypeArg string
	PayloadRoleArg  string
	AllFilter       bool
	ForceColor      bool
	ForceNoColor    bool
	RawFile         bool
	Out             io.Writer
	OutFile         *os.File
}

// Run renders a session log according to the provided options.
func Run(parser model.Parser, opts Options) error {
	if opts.Out == nil {
		opts.Out = os.Stdout
	}

	if opts.RawFile {
		return copyFile(opts.Out, opts.Path)
	}

	filters, err := buildViewFilters(opts.AllFilter, opts.EntryTypeArg, opts.ResponseTypeArg, opts.EventMsgTypeArg, opts.PayloadRoleArg)
	if err != nil {
		return err
	}

	formatMode := strings.ToLower(opts.Format)
	if formatMode == "" {
		formatMode = "text"
	}

	if _, err := parser.ReadSessionMeta(opts.Path); err != nil {
		return err
	}

	processEvents := func(fn func(model.EventProvider) error) error {
		return parser.IterateEvents(opts.Path, func(event model.EventProvider) error {
			if !eventMatchesFilters(event, filters) {
				return nil
			}
			return fn(event)
		})
	}

	switch formatMode {
	case "text":
		useColor := resolveColorChoice(opts)
		if opts.MaxEvents == 0 {
			count := 0
			return processEvents(func(event model.EventProvider) error {
				if count > 0 {
					fmt.Fprintln(opts.Out) //nolint:errcheck
				}
				printEvent(opts.Out, event, count+1, opts.Wrap, useColor)
				count++
				return nil
			})
		}
		ring := newEventRing(opts.MaxEvents)
		if err := processEvents(func(event model.EventProvider) error {
			ring.push(event)
			return nil
		}); err != nil {
			return err
		}
		for idx, event := range ring.slice() {
			if idx > 0 {
				fmt.Fprintln(opts.Out) //nolint:errcheck
			}
			printEvent(opts.Out, event, idx+1, opts.Wrap, useColor)
		}
		return nil

	case "raw":
		if opts.MaxEvents == 0 {
			return processEvents(func(event model.EventProvider) error {
				_, err := fmt.Fprintln(opts.Out, event.GetRaw()) //nolint:errcheck
				return err
			})
		}
		ring := newEventRing(opts.MaxEvents)
		if err := processEvents(func(event model.EventProvider) error {
			ring.push(event)
			return nil
		}); err != nil {
			return err
		}
		for _, event := range ring.slice() {
			fmt.Fprintln(opts.Out, event.GetRaw()) //nolint:errcheck
		}
		return nil

	case "chat":
		colorEnabled := resolveColorChoice(opts)
		width := determineWidth(opts.OutFile, opts.Wrap)

		var events []model.EventProvider
		if opts.MaxEvents > 0 {
			ring := newEventRing(opts.MaxEvents)
			if err := processEvents(func(event model.EventProvider) error {
				ring.push(event)
				return nil
			}); err != nil {
				return err
			}
			events = ring.slice()
		} else {
			collected := make([]model.EventProvider, 0)
			if err := processEvents(func(event model.EventProvider) error {
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
		if opts.OutFile != nil && isatty.IsTerminal(opts.OutFile.Fd()) {
			return pipeThroughPager(lines, colorEnabled)
		}
		return writeLines(opts.Out, lines)

	default:
		return fmt.Errorf("unsupported format: %s", opts.Format)
	}
}

type viewFilters struct {
	// TODO: Implement agent-agnostic filtering
	// For now, filters are disabled
	entryTypes        map[string]struct{}
	responseItemTypes map[string]struct{}
	eventMsgTypes     map[string]struct{}
	payloadRoles      map[string]struct{}
}

func buildViewFilters(allFilter bool, entryArg, responseTypeArg, eventMsgTypeArg, payloadRoleArg string) (viewFilters, error) {
	var filters viewFilters

	// If --all is specified, disable all filters
	if allFilter {
		return viewFilters{
			entryTypes:        nil,
			responseItemTypes: nil,
			eventMsgTypes:     nil,
			payloadRoles:      nil,
		}, nil
	}

	entryFilter, entryProvided, err := parseEntryTypeArg(entryArg)
	if err != nil {
		return filters, err
	}
	responseTypeFilter, responseTypeProvided, err := parseResponseTypeArg(responseTypeArg)
	if err != nil {
		return filters, err
	}
	eventMsgTypeFilter, eventMsgTypeProvided, err := parseEventMsgTypeArg(eventMsgTypeArg)
	if err != nil {
		return filters, err
	}
	payloadRoleFilter, roleProvided, err := parsePayloadRoleArg(payloadRoleArg)
	if err != nil {
		return filters, err
	}

	if entryProvided {
		filters.entryTypes = entryFilter
	} else {
		filters.entryTypes = map[string]struct{}{
			"response_item": {},
		}
	}

	if responseTypeProvided {
		filters.responseItemTypes = responseTypeFilter
	} else {
		filters.responseItemTypes = map[string]struct{}{
			"message": {},
		}
	}

	if eventMsgTypeProvided {
		filters.eventMsgTypes = eventMsgTypeFilter
	} else {
		// Default: no event_msg types (since EntryTypeEventMsg is excluded by default)
		filters.eventMsgTypes = nil
	}

	if roleProvided {
		filters.payloadRoles = payloadRoleFilter
	} else {
		filters.payloadRoles = map[string]struct{}{
			"user":      {},
			"assistant": {},
		}
	}

	return filters, nil
}

func parseEntryTypeArg(arg string) (map[string]struct{}, bool, error) {
	values := parseCSV(arg)
	if len(values) == 0 {
		return nil, false, nil
	}
	if len(values) == 1 && values[0] == "all" {
		return nil, true, nil
	}

	// Valid entry types (agent-agnostic)
	validTypes := map[string]bool{
		"session_meta":  true,
		"response_item": true,
		"event_msg":     true,
		"turn_context":  true,
	}

	set := make(map[string]struct{}, len(values))
	for _, token := range values {
		if !validTypes[token] {
			return nil, true, fmt.Errorf("unknown entry type %q", token)
		}
		set[token] = struct{}{}
	}
	return set, true, nil
}

func parseResponseTypeArg(arg string) (map[string]struct{}, bool, error) {
	values := parseCSV(arg)
	if len(values) == 0 {
		return nil, false, nil
	}
	if len(values) == 1 && values[0] == "all" {
		return nil, true, nil
	}

	// Valid response types (agent-agnostic)
	validTypes := map[string]bool{
		"message":                 true,
		"reasoning":               true,
		"function_call":           true,
		"function_call_output":    true,
		"custom_tool_call":        true,
		"custom_tool_call_output": true,
	}

	set := make(map[string]struct{}, len(values))
	for _, token := range values {
		if !validTypes[token] {
			return nil, true, fmt.Errorf("unknown response type %q", token)
		}
		set[token] = struct{}{}
	}
	return set, true, nil
}

func parseEventMsgTypeArg(arg string) (map[string]struct{}, bool, error) {
	values := parseCSV(arg)
	if len(values) == 0 {
		return nil, false, nil
	}
	if len(values) == 1 && values[0] == "all" {
		return nil, true, nil
	}

	// Valid event_msg types (agent-agnostic)
	validTypes := map[string]bool{
		"token_count":     true,
		"agent_reasoning": true,
		"user_message":    true,
		"agent_message":   true,
		"turn_aborted":    true,
	}

	set := make(map[string]struct{}, len(values))
	for _, token := range values {
		if !validTypes[token] {
			return nil, true, fmt.Errorf("unknown event_msg type %q", token)
		}
		set[token] = struct{}{}
	}
	return set, true, nil
}

func parsePayloadRoleArg(arg string) (map[string]struct{}, bool, error) {
	values := parseCSV(arg)
	if len(values) == 0 {
		return nil, false, nil
	}
	if len(values) == 1 && values[0] == "all" {
		return nil, true, nil
	}

	// Valid payload roles (agent-agnostic)
	validRoles := map[string]bool{
		"user":      true,
		"assistant": true,
		"tool":      true,
		"system":    true,
	}

	set := make(map[string]struct{}, len(values))
	for _, token := range values {
		if !validRoles[token] {
			return nil, true, fmt.Errorf("unknown payload role %q", token)
		}
		set[token] = struct{}{}
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

func eventMatchesFilters(_ model.EventProvider, _ viewFilters) bool {
	// TODO: Implement agent-agnostic filtering
	// For now, accept all events when using generic interface
	// Agent-specific filtering will be re-implemented later
	return true
}

type eventRing struct {
	data   []model.EventProvider
	start  int
	length int
}

func newEventRing(capacity int) *eventRing {
	if capacity <= 0 {
		return &eventRing{}
	}
	return &eventRing{data: make([]model.EventProvider, capacity)}
}

func (r *eventRing) push(event model.EventProvider) {
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

func (r *eventRing) slice() []model.EventProvider {
	if r.length == 0 {
		return nil
	}
	result := make([]model.EventProvider, r.length)
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
		defer stdin.Close()         //nolint:errcheck
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

func printEvent(out io.Writer, event model.EventProvider, index int, wrap int, useColor bool) {
	roleLabel := event.GetRole()
	if roleLabel == "" {
		roleLabel = "event"
	}
	roleLabel = strings.ToLower(roleLabel)

	ts := "-"
	if !event.GetTimestamp().IsZero() {
		ts = event.GetTimestamp().Format(time.RFC3339)
	}
	headerPlain := fmt.Sprintf("[#%03d] %s | %s", index, roleLabel, ts)

	indexText := fmt.Sprintf("#%03d", index)
	roleText := roleLabel
	tsText := ts
	separator := "|"

	if useColor {
		indexText = colorize(ansiBoldWhite, indexText)
		roleText = colorize(roleColor(roleLabel), roleText)
		tsText = colorize(ansiTimestamp, tsText)
		separator = colorize(ansiSeparator, "|")
	}

	header := fmt.Sprintf("[%s] %s %s %s", indexText, roleText, separator, tsText)
	fmt.Fprintln(out, header)                                //nolint:errcheck
	fmt.Fprintln(out, strings.Repeat("-", len(headerPlain))) //nolint:errcheck

	lines := format.RenderEventLines(event, wrap)
	if len(lines) == 0 {
		prefix := "|"
		if useColor {
			prefix = colorize(ansiSeparator, "|")
		}
		fmt.Fprintf(out, "%s %s\n", prefix, "(no content)") //nolint:errcheck
		return
	}
	linePrefix := "| "
	emptyPrefix := "|"
	if useColor {
		separatorColor := colorize(ansiSeparator, "|")
		linePrefix = separatorColor + " "
		emptyPrefix = separatorColor
	}
	for _, line := range lines {
		if line == "" {
			fmt.Fprintln(out, emptyPrefix) //nolint:errcheck
			continue
		}
		fmt.Fprintf(out, "%s%s\n", linePrefix, line) //nolint:errcheck
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

func colorize(code string, text string) string {
	return code + text + ansiReset
}

func roleColor(role string) string {
	switch role {
	case "assistant":
		return ansiAssistant
	case "user":
		return ansiUser
	case "tool", "system":
		return ansiTool
	default:
		return ansiSeparator
	}
}

func resolveColorChoice(opts Options) bool {
	if opts.ForceColor {
		return true
	}
	if opts.ForceNoColor {
		return false
	}
	return shouldUseColorAuto(opts.Out)
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

func copyFile(dst io.Writer, path string) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close() //nolint:errcheck

	_, err = io.Copy(dst, f)
	return err
}
