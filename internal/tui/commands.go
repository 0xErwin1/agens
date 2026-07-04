package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

// maxPaletteItems caps how many command rows the palette shows at once so it
// never grows taller than the space the layout reserves for it.
const maxPaletteItems = 6

// CommandContext is the narrow surface a command acts on. Commands depend on
// this interface rather than on *Model, so a command never reaches into the
// model's internals and the two stay decoupled: adding a command is writing a
// closure against these few operations.
type CommandContext interface {
	// NewConversation discards the current history and clears the view.
	NewConversation()
	// Notify appends a muted, system-level note to the conversation.
	Notify(text string)
	// CurrentModel returns the active model's display name.
	CurrentModel() string
	// CommandHelp returns the help text listing commands and key bindings.
	CommandHelp() string
}

// CommandFunc runs a command against a context and optionally returns a
// tea.Cmd (for example tea.Quit). It must not block.
type CommandFunc func(ctx CommandContext) tea.Cmd

// Command is one slash command: how it is named and described in the palette,
// and what it does when run.
type Command struct {
	Name string
	Desc string
	Run  CommandFunc
}

// CommandRegistry holds the available slash commands, ordered for display and
// indexed by name for lookup. Register makes the set extensible at runtime
// (e.g. future skills) without editing the model.
type CommandRegistry struct {
	ordered []Command
	byName  map[string]Command
}

// NewCommandRegistry builds a registry from cmds, preserving their order.
func NewCommandRegistry(cmds ...Command) *CommandRegistry {
	r := &CommandRegistry{byName: make(map[string]Command, len(cmds))}
	for _, c := range cmds {
		r.Register(c)
	}
	return r
}

// Register adds or replaces a command by name, keeping display order stable:
// a new name is appended, an existing one is updated in place.
func (r *CommandRegistry) Register(c Command) {
	if _, exists := r.byName[c.Name]; !exists {
		r.ordered = append(r.ordered, c)
	} else {
		for i := range r.ordered {
			if r.ordered[i].Name == c.Name {
				r.ordered[i] = c
				break
			}
		}
	}
	r.byName[c.Name] = c
}

// All returns the commands in display order.
func (r *CommandRegistry) All() []Command { return r.ordered }

// Match returns the commands whose name starts with input's command token. A
// bare "/" matches all; non-command input matches none.
func (r *CommandRegistry) Match(input string) []Command {
	token := commandToken(input)
	if token == "" {
		return nil
	}

	out := make([]Command, 0, len(r.ordered))
	for _, c := range r.ordered {
		if strings.HasPrefix(c.Name, token) {
			out = append(out, c)
		}
	}
	return out
}

// Lookup returns the command exactly named by input's token.
func (r *CommandRegistry) Lookup(input string) (Command, bool) {
	c, ok := r.byName[commandToken(input)]
	return c, ok
}

// Help renders the command list, one "  /name  desc" line per command in
// display order. Callers append their own key-binding section.
func (r *CommandRegistry) Help() string {
	width := 0
	for _, c := range r.ordered {
		if len(c.Name) > width {
			width = len(c.Name)
		}
	}

	lines := make([]string, 0, len(r.ordered))
	for _, c := range r.ordered {
		lines = append(lines, "  "+padRight(c.Name, width)+"  "+c.Desc)
	}
	return strings.Join(lines, "\n")
}

// defaultCommands is the built-in slash-command set. Each command is a
// self-contained closure over CommandContext; there is no central switch.
func defaultCommands() *CommandRegistry {
	return NewCommandRegistry(
		Command{Name: "/new", Desc: "empezar un chat nuevo", Run: func(ctx CommandContext) tea.Cmd {
			ctx.NewConversation()
			return nil
		}},
		Command{Name: "/clear", Desc: "limpiar la conversación", Run: func(ctx CommandContext) tea.Cmd {
			ctx.NewConversation()
			return nil
		}},
		Command{Name: "/model", Desc: "mostrar el modelo actual", Run: func(ctx CommandContext) tea.Cmd {
			ctx.Notify("modelo actual: " + ctx.CurrentModel())
			return nil
		}},
		Command{Name: "/help", Desc: "ver comandos y atajos", Run: func(ctx CommandContext) tea.Cmd {
			ctx.Notify(ctx.CommandHelp())
			return nil
		}},
		Command{Name: "/quit", Desc: "salir de agens", Run: func(CommandContext) tea.Cmd {
			return tea.Quit
		}},
	)
}

// keyBindingsHelp is the static key-binding section appended to /help.
func keyBindingsHelp() string {
	return strings.Join([]string{
		"atajos:",
		"  enter        enviar",
		"  ctrl+c       cancelar turno / salir",
		"  pgup / pgdn  scroll de la conversación",
		"  esc          cerrar palette / rechazar permiso",
	}, "\n")
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

// padRight pads s with spaces to at least width columns.
func padRight(s string, width int) string {
	if len(s) >= width {
		return s
	}
	return s + strings.Repeat(" ", width-len(s))
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

// renderPalette draws the command palette: one row per matched command, the
// selected row marked and highlighted, inside a bordered box sized to width.
func renderPalette(items []Command, selected, width int) string {
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

		name := lipgloss.NewStyle().Foreground(nameColor).Bold(true).Width(10).Render(c.Name)
		desc := lipgloss.NewStyle().Foreground(theme.Muted()).Render(c.Desc)

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
