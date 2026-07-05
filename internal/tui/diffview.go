package tui

import (
	"fmt"
	"strconv"
	"strings"

	"github.com/charmbracelet/lipgloss"
)

// isDiffResult reports whether a tool result is a unified diff produced by the
// fs edit/write tools, so it can be rendered as a friendly colored diff rather
// than plain text.
func isDiffResult(s string) bool {
	return strings.HasPrefix(s, "--- a/") && strings.Contains(s, "\n@@ ")
}

// renderDiffBody renders a unified diff as a friendly, GitHub-style view: the
// git file and hunk headers are dropped, and each changed line is tinted (green
// additions, red deletions) and gutter-numbered by its line in the new file.
// The body is only rendered when its tool block is expanded, and it is shown in
// full — a long diff is scrolled by the conversation viewport rather than
// truncated, so the whole change can be reviewed.
func (m *Messages) renderDiffBody(diff string) string {
	theme := CurrentTheme()

	width := m.width - contentGutter
	if width < 1 {
		width = 1
	}

	rows := make([]string, 0)
	newLine := 0

	for _, ln := range strings.Split(diff, "\n") {
		if strings.HasPrefix(ln, "--- ") || strings.HasPrefix(ln, "+++ ") {
			continue
		}
		if strings.HasPrefix(ln, "@@") {
			newLine = parseHunkNewStart(ln)
			continue
		}

		switch {
		case strings.HasPrefix(ln, "+"):
			rows = append(rows, diffRow(theme, newLine, '+', ln[1:], width))
			newLine++
		case strings.HasPrefix(ln, "-"):
			rows = append(rows, diffRow(theme, 0, '-', ln[1:], width))
		case strings.HasPrefix(ln, " "):
			rows = append(rows, diffRow(theme, newLine, ' ', ln[1:], width))
			newLine++
		}
	}

	return lipgloss.NewStyle().MarginLeft(contentGutter).Render(strings.Join(rows, "\n"))
}

// diffRow renders one diff line: a right-aligned line number (blank for a
// deletion, which has no line in the new file), the change marker, and the
// content, tinted by kind and padded to width so additions and deletions read
// as solid colored rows.
func diffRow(theme Theme, num int, sign rune, content string, width int) string {
	content = strings.ReplaceAll(content, "\t", "  ")

	numCol := "    "
	if num > 0 {
		numCol = fmt.Sprintf("%4d", num)
	}

	text := fitWidth(numCol+" "+string(sign)+" "+content, width)

	style := lipgloss.NewStyle()
	switch sign {
	case '+':
		style = style.Foreground(theme.User()).Background(theme.DiffAddBg())
	case '-':
		style = style.Foreground(theme.Error()).Background(theme.DiffRemoveBg())
	default:
		style = style.Foreground(theme.Muted())
	}
	return style.Render(text)
}

// parseHunkNewStart extracts the new-file starting line from a hunk header like
// "@@ -12,3 +14,4 @@", returning 0 when it cannot be parsed.
func parseHunkNewStart(header string) int {
	plus := strings.IndexByte(header, '+')
	if plus < 0 {
		return 0
	}

	rest := header[plus+1:]
	end := 0
	for end < len(rest) && rest[end] >= '0' && rest[end] <= '9' {
		end++
	}

	n, _ := strconv.Atoi(rest[:end])
	return n
}

// fitWidth truncates or space-pads s to exactly width columns so a tinted row
// fills the content column. Width is counted in runes, which suffices for the
// code diffs shown here.
func fitWidth(s string, width int) string {
	r := []rune(s)
	if len(r) > width {
		return string(r[:width])
	}
	return s + strings.Repeat(" ", width-len(r))
}
