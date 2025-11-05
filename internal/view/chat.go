package view

import (
	"fmt"
	"regexp"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"codexlog/internal/format"
	"codexlog/internal/model"

	"github.com/mattn/go-runewidth"
)

func renderChatTranscript(events []model.Event, width int, useColor bool) []string {
	if width <= 0 {
		width = 80
	}
	padding := 2

	lines := make([]string, 0, len(events)*6)
	for idx, event := range events {
		if idx > 0 {
			lines = append(lines, "")
		}
		lines = append(lines, renderChatBubble(event, width, padding, useColor)...)
	}
	return lines
}

func renderChatBubble(event model.Event, totalWidth int, padding int, useColor bool) []string {
	role := strings.ToLower(roleLabel(event))
	bodyLines := format.RenderEventLines(event, 0)

	maxContentWidth := totalWidth - padding*2 - 10
	if maxContentWidth < 20 {
		if totalWidth > 30 {
			maxContentWidth = totalWidth - 12
		} else {
			maxContentWidth = totalWidth - 8
		}
		if maxContentWidth < 8 {
			maxContentWidth = 8
		}
	}

	headerText, headerLabel, headerTime := chatHeader(role, event.Timestamp)
	content := wrapLines(append([]string{headerText}, bodyLines...), maxContentWidth)
	maxLineWidth := contentMaxWidth(content)

	bubbleWidth := maxLineWidth
	if bubbleWidth > maxContentWidth {
		bubbleWidth = maxContentWidth
	}

	align := alignmentForRole(role)
	leftPad := computeLeftPad(totalWidth, bubbleWidth, padding, align)

	if useColor && len(content) > 0 {
		colored := fmt.Sprintf("%s · %s",
			colorize(true, roleColor(role), headerLabel),
			colorize(true, ansiTimestamp, headerTime),
		)
		content[0] = strings.Replace(content[0], headerText, colored, 1)
	}

	top := fmt.Sprintf("%s╭%s╮", strings.Repeat(" ", leftPad), strings.Repeat("─", bubbleWidth+2))
	bottom := fmt.Sprintf("%s╰%s╯", strings.Repeat(" ", leftPad), strings.Repeat("─", bubbleWidth+2))

	result := []string{top}
	for _, line := range content {
		result = append(result, renderBubbleBodyLine(line, bubbleWidth, leftPad, useColor))
	}
	result = append(result, bottom)
	return result
}

func renderBubbleBodyLine(line string, bubbleWidth int, leftPad int, useColor bool) string {
	displayLen := visibleWidth(line)
	if displayLen > bubbleWidth {
		line = truncateToWidth(line, bubbleWidth)
		displayLen = bubbleWidth
	}
	paddingRight := bubbleWidth - displayLen

	border := "|"
	if useColor {
		border = colorize(true, ansiSeparator, border)
	}

	return fmt.Sprintf("%s%s %s%s %s", strings.Repeat(" ", leftPad), border, line, strings.Repeat(" ", paddingRight), border)
}

func chatHeader(role string, ts time.Time) (header string, label string, timeText string) {
	label = titleCase(role)
	if label == "" {
		label = "Event"
	}
	timeText = "-"
	if !ts.IsZero() {
		timeText = ts.Format("Jan 02 15:04")
	}

	return fmt.Sprintf("%s · %s", label, timeText), label, timeText
}

func roleLabel(event model.Event) string {
	if event.Role != "" {
		return string(event.Role)
	}
	if event.Kind != "" {
		kind := string(event.Kind)
		// For event_msg and turn_context, show the specific type
		if (kind == "event_msg" || kind == "turn_context") && event.PayloadType != "" {
			return kind + ": " + event.PayloadType
		}
		return kind
	}
	if event.PayloadType != "" {
		return event.PayloadType
	}
	return "event"
}

func alignmentForRole(role string) string {
	// Check for system/metadata entries first (by string comparison)
	switch role {
	case "session_meta", "event_msg", "turn_context":
		return "left"
	}

	// Then check for role-based alignment
	switch model.PayloadRole(role) {
	case model.PayloadRoleAssistant:
		return "left"
	case model.PayloadRoleTool, model.PayloadRoleSystem:
		return "center"
	case model.PayloadRoleUser:
		return "right"
	default:
		// Unknown types default to left
		return "left"
	}
}

func computeLeftPad(totalWidth, bubbleWidth, padding int, align string) int {
	maxPad := totalWidth - bubbleWidth - 4
	if maxPad < 0 {
		maxPad = 0
	}

	switch align {
	case "right":
		if maxPad < padding {
			return maxPad
		}
		return maxPad
	case "center":
		center := maxPad / 2
		if center < padding {
			center = padding
		}
		if center > maxPad {
			center = maxPad
		}
		return center
	default:
		if padding > maxPad {
			return maxPad
		}
		return padding
	}
}

func wrapLines(lines []string, width int) []string {
	var out []string
	for _, line := range lines {
		out = append(out, wrapText(line, width)...)
	}
	return out
}

func wrapText(text string, width int) []string {
	if width <= 0 {
		return []string{text}
	}
	text = strings.TrimRight(text, " ")
	if text == "" {
		return []string{""}
	}
	var out []string
	var current strings.Builder
	currentWidth := 0

	for _, r := range text {
		rw := runewidth.RuneWidth(r)
		if currentWidth+rw > width && current.Len() > 0 {
			out = append(out, current.String())
			current.Reset()
			currentWidth = 0
		}
		current.WriteRune(r)
		currentWidth += rw
	}
	if currentWidth > 0 || current.Len() > 0 {
		out = append(out, current.String())
	}
	return out
}

func titleCase(text string) string {
	runes := []rune(text)
	if len(runes) == 0 {
		return ""
	}
	runes[0] = unicode.ToUpper(runes[0])
	for i := 1; i < len(runes); i++ {
		runes[i] = unicode.ToLower(runes[i])
	}
	return string(runes)
}

func contentMaxWidth(lines []string) int {
	max := 0
	for _, line := range lines {
		if w := visibleWidth(line); w > max {
			max = w
		}
	}
	return max
}

func truncateToWidth(text string, width int) string {
	if visibleWidth(text) <= width {
		return text
	}
	var stripped strings.Builder
	var colored strings.Builder
	current := 0

	for i := 0; i < len(text); {
		if m := ansiPattern.FindStringIndex(text[i:]); m != nil && m[0] == 0 {
			seq := text[i : i+m[1]]
			colored.WriteString(seq)
			i += m[1]
			continue
		}
		r, size := utf8.DecodeRuneInString(text[i:])
		rw := runewidth.RuneWidth(r)
		if current+rw > width {
			break
		}
		colored.WriteRune(r)
		stripped.WriteRune(r)
		current += rw
		i += size
	}
	return colored.String()
}

var ansiPattern = regexp.MustCompile(`\x1b\[[0-9;]*m`)

func visibleWidth(text string) int {
	clean := ansiPattern.ReplaceAllString(text, "")
	return runewidth.StringWidth(clean)
}
