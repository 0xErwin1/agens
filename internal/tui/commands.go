package tui

import (
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	usercommand "github.com/0xErwin1/agens/internal/command"
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
	// OpenModelSelector opens the interactive model selector, returning the
	// command that fetches the catalog (or nil when unavailable).
	OpenModelSelector() tea.Cmd
	// OpenEffortSelector opens the reasoning-effort selector.
	OpenEffortSelector() tea.Cmd
	// OpenSessionPicker opens the saved-conversation picker, returning the
	// command that lists them (or nil when unavailable).
	OpenSessionPicker() tea.Cmd
	// OpenSubagentTree opens the active-subagent tree overlay.
	OpenSubagentTree() tea.Cmd
	// OpenAgentMenu opens the agents menu, where each subagent's available models
	// are defined.
	OpenAgentMenu() tea.Cmd
	// OpenAgentPicker opens the primary-agent picker used to switch the active
	// agent.
	OpenAgentPicker() tea.Cmd
	// ToggleMode sets or toggles the live chat/edit operating mode: arg is
	// "chat", "edit", or "" to flip between the two. It returns a note (and
	// leaves the mode unchanged) when either no ModeState is wired or arg
	// names neither mode.
	ToggleMode(arg string) tea.Cmd
	// ToggleMouse flips mouse reporting so the user can select/copy text from the
	// conversation (mouse off) or scroll with the wheel (mouse on).
	ToggleMouse() tea.Cmd
	// CommandHelp returns the help text listing commands and key bindings.
	CommandHelp() string
	// SubmitUserPrompt submits expanded user-authored command text as a normal turn.
	SubmitUserPrompt(text string) tea.Cmd
}

// CommandFunc runs a command against a context and the original input, and
// optionally returns a tea.Cmd (for example tea.Quit). It must not block.
type CommandFunc func(ctx CommandContext, input string) tea.Cmd

// Command is one slash command: how it is named and described in the palette,
// and what it does when run. SafeWhileRunning marks a command that neither
// mutates the in-flight turn nor the agent loop's live settings, so it may run
// while a turn is executing; the rest are offered only when idle.
type Command struct {
	Name             string
	Desc             string
	Run              CommandFunc
	SafeWhileRunning bool
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

// MatchRunnable is Match narrowed to the commands offered in the current state:
// while a turn is running only SafeWhileRunning commands are shown, so the
// palette never suggests a command that cannot run mid-turn.
func (r *CommandRegistry) MatchRunnable(input string, running bool) []Command {
	matched := r.Match(input)
	if !running {
		return matched
	}

	out := make([]Command, 0, len(matched))
	for _, c := range matched {
		if c.SafeWhileRunning {
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
		Command{Name: "/new", Desc: "start a new chat", Run: func(ctx CommandContext, _ string) tea.Cmd {
			ctx.NewConversation()
			return nil
		}},
		Command{Name: "/clear", Desc: "clear the conversation", Run: func(ctx CommandContext, _ string) tea.Cmd {
			ctx.NewConversation()
			return nil
		}},
		Command{Name: "/model", Desc: "choose the model", Run: func(ctx CommandContext, _ string) tea.Cmd {
			return ctx.OpenModelSelector()
		}},
		Command{Name: "/effort", Desc: "set the reasoning effort", Run: func(ctx CommandContext, _ string) tea.Cmd {
			return ctx.OpenEffortSelector()
		}},
		Command{Name: "/sessions", Desc: "resume a saved conversation", Run: func(ctx CommandContext, _ string) tea.Cmd {
			return ctx.OpenSessionPicker()
		}},
		Command{Name: "/subagents", Desc: "list active subagents", SafeWhileRunning: true, Run: func(ctx CommandContext, _ string) tea.Cmd {
			return ctx.OpenSubagentTree()
		}},
		Command{Name: "/agent", Desc: "switch the active agent (or press tab)", Run: func(ctx CommandContext, _ string) tea.Cmd {
			return ctx.OpenAgentPicker()
		}},
		Command{Name: "/agents", Desc: "define each subagent's available models", Run: func(ctx CommandContext, _ string) tea.Cmd {
			return ctx.OpenAgentMenu()
		}},
		Command{Name: "/mode", Desc: "toggle chat/edit mode: [chat|edit], blank to flip", Run: func(ctx CommandContext, input string) tea.Cmd {
			return ctx.ToggleMode(commandArguments(input))
		}},
		Command{Name: "/select", Desc: "toggle mouse off to select & copy text", SafeWhileRunning: true, Run: func(ctx CommandContext, _ string) tea.Cmd {
			return ctx.ToggleMouse()
		}},
		Command{Name: "/help", Desc: "show commands and shortcuts", SafeWhileRunning: true, Run: func(ctx CommandContext, _ string) tea.Cmd {
			ctx.Notify(ctx.CommandHelp())
			return nil
		}},
		Command{Name: "/quit", Desc: "quit agens", SafeWhileRunning: true, Run: func(CommandContext, string) tea.Cmd {
			return tea.Quit
		}},
	)
}

func registerUserCommands(registry *CommandRegistry, set *usercommand.Set) {
	if set == nil {
		return
	}
	for _, userCmd := range set.All() {
		cmd := userCmd
		name := "/" + cmd.Name
		if _, exists := registry.byName[name]; exists {
			continue
		}
		registry.Register(Command{
			Name: name,
			Desc: commandDescription(cmd),
			Run: func(ctx CommandContext, input string) tea.Cmd {
				return ctx.SubmitUserPrompt(cmd.Expand(commandArguments(input)))
			},
		})
	}
}

func commandDescription(cmd usercommand.Command) string {
	desc := cmd.Description
	if desc == "" {
		desc = "user command"
	}
	if cmd.ArgumentHint != "" {
		desc += " " + cmd.ArgumentHint
	}
	return desc
}

func commandArguments(input string) string {
	trimmed := strings.TrimSpace(input)
	if i := strings.IndexByte(trimmed, ' '); i >= 0 {
		return strings.TrimSpace(trimmed[i+1:])
	}
	return ""
}

// keyBindingsHelp is the static key-binding section appended to /help.
func keyBindingsHelp() string {
	return strings.Join([]string{
		"shortcuts:",
		"  enter        send",
		"  tab          rotate the active agent",
		"  ctrl+c       cancel turn / quit",
		"  ctrl+↑       subagents: list (↑/↓ · enter open · esc back)",
		"  ctrl+o       expand / collapse tool output & thinking",
		"  ctrl+p       toggle detailed token usage",
		"  pgup / pgdn  scroll the conversation",
		"  esc          close palette / deny permission",
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
