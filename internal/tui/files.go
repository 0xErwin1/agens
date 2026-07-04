package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// maxFileRows caps how many file rows the picker shows at once; a longer list
// scrolls a window that follows the selection.
const maxFileRows = 8

// maxFileMatches bounds how many filtered matches are kept, so a bare "@" on a
// large repo does not build an unbounded list.
const maxFileMatches = 200

// maxFileRefBytes caps how much of a referenced file is inlined into the
// prompt, so a large file cannot blow up the request.
const maxFileRefBytes = 64 * 1024

// FileSource lists and reads the project's files for @-references. The CLI
// provides one confined to the project root; nil disables @-references.
type FileSource interface {
	List() ([]string, error)
	Read(path string) (string, error)
}

// filesLoadedMsg delivers the project file list, loaded once at startup, into
// the Update loop.
type filesLoadedMsg struct {
	files []string
	err   error
}

// loadFilesCmd lists the project files off the UI goroutine.
func loadFilesCmd(src FileSource) tea.Cmd {
	return func() tea.Msg {
		files, err := src.List()
		return filesLoadedMsg{files: files, err: err}
	}
}

// atToken returns the in-progress "@reference" at the end of value: the text
// after the last "@" that begins a word and has no whitespace after it. start
// is the index of the "@". ok is false when no reference is being typed.
func atToken(value string) (token string, start int, ok bool) {
	at := strings.LastIndexByte(value, '@')
	if at < 0 {
		return "", 0, false
	}
	if at > 0 && !isBoundary(value[at-1]) {
		return "", 0, false
	}

	rest := value[at+1:]
	if strings.ContainsAny(rest, " \t\n") {
		return "", 0, false
	}
	return rest, at, true
}

// isBoundary reports whether b can precede an @-reference (start-of-word).
func isBoundary(b byte) bool {
	return b == ' ' || b == '\t' || b == '\n'
}

// filterFiles returns the files matching query, prefix matches first, then
// substring matches, capped to maxFileMatches. An empty query returns the
// first files.
func filterFiles(files []string, query string) []string {
	if query == "" {
		return capFiles(files)
	}

	q := strings.ToLower(query)
	var prefix, substr []string
	for _, f := range files {
		lf := strings.ToLower(f)
		switch {
		case strings.HasPrefix(lf, q):
			prefix = append(prefix, f)
		case strings.Contains(lf, q):
			substr = append(substr, f)
		}
	}
	return capFiles(append(prefix, substr...))
}

func capFiles(files []string) []string {
	if len(files) > maxFileMatches {
		return files[:maxFileMatches]
	}
	return files
}

// extractFileRefs returns the referenced paths in text that name a known
// project file. known is the set of listed files, so plain "@mentions" that
// are not files are ignored.
func extractFileRefs(text string, known map[string]struct{}) []string {
	var refs []string
	seen := map[string]struct{}{}

	for _, field := range strings.Fields(text) {
		if !strings.HasPrefix(field, "@") {
			continue
		}
		path := strings.TrimPrefix(field, "@")
		if _, ok := known[path]; !ok {
			continue
		}
		if _, dup := seen[path]; dup {
			continue
		}
		seen[path] = struct{}{}
		refs = append(refs, path)
	}
	return refs
}

// renderFileSelector draws the @-reference picker: an indexing note while the
// file list loads, an empty note, or the matching files with the selection
// highlighted and the visible window following it.
func renderFileSelector(items []string, selected int, indexing bool, width int) string {
	theme := CurrentTheme()

	inner := width - 4
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Reference a file"))
	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("↑/↓ · tab · enter insert · esc cancel"))

	var body []string
	switch {
	case indexing:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("indexing files…"))}
	case len(items) == 0:
		body = []string{oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render("no matching files"))}
	default:
		body = fileRows(items, selected, theme, inner)
	}

	content := append([]string{title}, body...)
	content = append(content, "", hint)

	box := lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		BorderForeground(theme.Accent()).
		Padding(0, 1)
	if width > 4 {
		box = box.Width(width - 2)
	}

	return box.Render(strings.Join(content, "\n"))
}

func fileRows(items []string, selected int, theme Theme, inner int) []string {
	start := windowStart(selected, len(items), maxFileRows)
	end := start + maxFileRows
	if end > len(items) {
		end = len(items)
	}

	rows := make([]string, 0, end-start)
	for i := start; i < end; i++ {
		marker := "  "
		color := theme.Assistant()
		if i == selected {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			color = theme.User()
		}

		path := lipgloss.NewStyle().Foreground(color).Render(items[i])
		rows = append(rows, lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(marker+path))
	}
	return rows
}
