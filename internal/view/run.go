package view

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"time"

	"codexlog/internal/format"
	"codexlog/internal/model"
	"codexlog/internal/parser"

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
	PayloadTypeArg  string
	PayloadRoleArg  string
	AllFilter       bool
	ForceColor      bool
	ForceNoColor    bool
	RawFile         bool
	Out             io.Writer
	OutFile         *os.File
}

// Run renders a session log according to the provided options.
func Run(opts Options) error {
	if opts.Out == nil {
		opts.Out = os.Stdout
	}

	if opts.RawFile {
		return copyFile(opts.Out, opts.Path)
	}

	filters, err := buildViewFilters(opts.AllFilter, opts.EntryTypeArg, opts.PayloadTypeArg, opts.PayloadRoleArg)
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

	processEvents := func(fn func(model.Event) error) error {
		return parser.IterateEvents(opts.Path, func(event model.Event) error {
			// Skip session_meta unless --all is specified
			if !opts.AllFilter && event.Kind == model.EntryTypeSessionMeta {
				return nil
			}
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
			return processEvents(func(event model.Event) error {
				if count > 0 {
					fmt.Fprintln(opts.Out)
				}
				printEvent(opts.Out, event, count+1, opts.Wrap, useColor)
				count++
				return nil
			})
		}
		ring := newEventRing(opts.MaxEvents)
		if err := processEvents(func(event model.Event) error {
			ring.push(event)
			return nil
		}); err != nil {
			return err
		}
		for idx, event := range ring.slice() {
			if idx > 0 {
				fmt.Fprintln(opts.Out)
			}
			printEvent(opts.Out, event, idx+1, opts.Wrap, useColor)
		}
		return nil

	case "raw":
		if opts.MaxEvents == 0 {
			return processEvents(func(event model.Event) error {
				_, err := fmt.Fprintln(opts.Out, event.Raw)
				return err
			})
		}
		ring := newEventRing(opts.MaxEvents)
		if err := processEvents(func(event model.Event) error {
			ring.push(event)
			return nil
		}); err != nil {
			return err
		}
		for _, event := range ring.slice() {
			fmt.Fprintln(opts.Out, event.Raw)
		}
		return nil

	case "chat":
		colorEnabled := resolveColorChoice(opts)
		width := determineWidth(opts.OutFile, opts.Wrap)

		var events []model.Event
		if opts.MaxEvents > 0 {
			ring := newEventRing(opts.MaxEvents)
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
		if opts.OutFile != nil && isatty.IsTerminal(opts.OutFile.Fd()) {
			return pipeThroughPager(lines, colorEnabled)
		}
		return writeLines(opts.Out, lines)

	default:
		return fmt.Errorf("unsupported format: %s", opts.Format)
	}
}

type viewFilters struct {
	entryTypes   map[model.EntryType]struct{}
	payloadTypes map[model.PayloadType]struct{}
	payloadRoles map[model.PayloadRole]struct{}
}

func buildViewFilters(allFilter bool, entryArg, payloadTypeArg, payloadRoleArg string) (viewFilters, error) {
	var filters viewFilters

	// If --all is specified, disable all filters
	if allFilter {
		return viewFilters{
			entryTypes:   nil,
			payloadTypes: nil,
			payloadRoles: nil,
		}, nil
	}

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
	defer f.Close()

	_, err = io.Copy(dst, f)
	return err
}
