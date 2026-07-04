package tui

import (
	"strings"

	"github.com/charmbracelet/lipgloss"
)

// maxPaletteItems caps how many command rows the palette shows at once so it
// never grows taller than the space the layout reserves for it.
const maxPaletteItems = 6

// command is one slash command the palette can complete and run.
type command struct {
	name string
	desc string
}

// commands is the built-in slash-command set, in display order. Kept in a
// dedicated file (mirroring the reference's internal/commands package) so new
// commands are added here rather than threaded through the model.
var commands = []command{
	{"/new", "empezar un chat nuevo"},
	{"/clear", "limpiar la conversación"},
	{"/model", "mostrar el modelo actual"},
	{"/help", "ver comandos y atajos"},
	{"/quit", "salir de agens"},
}

// commandToken extracts the leading "/word" token from input, ignoring any
// arguments after the first space. It returns "" when input is not a command.
func commandToken(input string) string {
	token := strings.TrimSpace(input)
	if !strings.HasPrefix(token, "/") {
		return ""
	}
	if i := strings.IndexByte(token, ' '); i >= 0 {
		token = token[:i]
	}
	return token
}

// matchCommands returns the commands whose name starts with the input's
// command token. A bare "/" matches all commands; a non-command input matches
// none.
func matchCommands(input string) []command {
	token := commandToken(input)
	if token == "" {
		return nil
	}

	out := make([]command, 0, len(commands))
	for _, c := range commands {
		if strings.HasPrefix(c.name, token) {
			out = append(out, c)
		}
	}
	return out
}

// lookupCommand returns the command exactly named by input's token.
func lookupCommand(input string) (command, bool) {
	token := commandToken(input)
	for _, c := range commands {
		if c.name == token {
			return c, true
		}
	}
	return command{}, false
}

// paletteHeight is the number of terminal rows the palette occupies for n
// matched commands, including its border. It is zero when there is nothing to
// show, so the layout reserves no space.
func paletteHeight(n int) int {
	if n <= 0 {
		return 0
	}
	if n > maxPaletteItems {
		n = maxPaletteItems
	}
	return n + 2 // rounded border top and bottom
}

// helpText is the body shown by /help: the command list and the key bindings.
func helpText() string {
	return strings.Join([]string{
		"comandos:",
		"  /new     empezar un chat nuevo",
		"  /clear   limpiar la conversación",
		"  /model   mostrar el modelo actual",
		"  /help    esta ayuda",
		"  /quit    salir",
		"",
		"atajos:",
		"  enter        enviar",
		"  ctrl+c       cancelar turno / salir",
		"  pgup / pgdn  scroll de la conversación",
		"  esc          cerrar palette / rechazar permiso",
	}, "\n")
}

// renderPalette draws the command palette: one row per matched command, the
// selected row marked and highlighted, inside a bordered box sized to width.
func renderPalette(items []command, selected, width int) string {
	theme := CurrentTheme()

	if len(items) > maxPaletteItems {
		items = items[:maxPaletteItems]
	}

	inner := width - 4 // border (2) + horizontal padding (2)
	if inner < 8 {
		inner = 8
	}

	rows := make([]string, 0, len(items))
	for i, c := range items {
		marker := "  "
		nameColor := theme.Accent()
		if i == selected {
			marker = lipgloss.NewStyle().Foreground(theme.User()).Render("› ")
			nameColor = theme.User()
		}

		name := lipgloss.NewStyle().Foreground(nameColor).Bold(true).Width(10).Render(c.name)
		desc := lipgloss.NewStyle().Foreground(theme.Muted()).Render(c.desc)

		rows = append(rows, lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(marker+name+desc))
	}

	box := lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		BorderForeground(theme.Accent()).
		Padding(0, 1)
	if width > 4 {
		box = box.Width(width - 2)
	}

	return box.Render(strings.Join(rows, "\n"))
}
