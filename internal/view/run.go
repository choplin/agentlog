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
	Path         string
	Format       string
	Wrap         int
	MaxEvents    int
	Level        string // "conversation" or "all"
	ForceColor   bool
	ForceNoColor bool
	RawFile      bool
	Out          io.Writer
	OutFile      *os.File
}

// Run renders a session log according to the provided options.
func Run(parser model.Parser, opts Options) error {
	if opts.Out == nil {
		opts.Out = os.Stdout
	}

	if opts.RawFile {
		return copyFile(opts.Out, opts.Path)
	}

	// Normalize level to lowercase, default to "conversation"
	level := strings.ToLower(opts.Level)
	if level == "" {
		level = "conversation"
	}

	// Validate level
	showOnlyConversation := false
	switch level {
	case "conversation":
		showOnlyConversation = true
	case "all":
		showOnlyConversation = false
	default:
		return fmt.Errorf("invalid level %q: must be 'conversation' or 'all'", opts.Level)
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
			// Filter by conversation level
			if showOnlyConversation && !event.IsConversation() {
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
