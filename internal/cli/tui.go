package cli

import (
	"fmt"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/spf13/cobra"

	"github.com/iperez/agens/internal/agent"
	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/permission"
	"github.com/iperez/agens/internal/tui"
)

// tuiSession is everything the TUI needs from the build step: the agent loop,
// the model lister for the /model selector, the system-prompt rebuilder used
// on a live model switch, and the model name for the status line.
type tuiSession struct {
	loop   *agentloop.Loop
	lister tui.ModelLister
	prompt tui.SystemPromptFunc
	model  string
}

// tuiLoopBuilder resolves an agent.Options into a tuiSession. It is the
// dependency-injection seam that lets tests exercise the command against a
// fake provider without touching config, auth, or the network.
type tuiLoopBuilder func(opts agent.Options) (tuiSession, error)

// tuiRunner starts the interactive program for the given root model. It is a
// second seam, distinct from the builder, so tests can verify the command
// wiring without starting a Bubble Tea program (which requires a TTY).
type tuiRunner func(model tea.Model) error

func newTUICommand() *cobra.Command {
	return newTUICommandWithBuilder(defaultBuildTUI, defaultRunTUI)
}

func newTUICommandWithBuilder(build tuiLoopBuilder, run tuiRunner) *cobra.Command {
	var opts agent.Options
	var allowAll bool

	cmd := &cobra.Command{
		Use:   "tui",
		Short: "Start the interactive terminal UI",
		Long:  "Start the interactive terminal UI: a full-screen conversation view with a prompt input, streaming responses, and tool activity.",
		Args:  cobra.NoArgs,
		RunE: func(_ *cobra.Command, _ []string) error {
			// The TUI owns the terminal, so it cannot use the tty-reading
			// prompter the chat command does: an interactive decision is
			// routed through the Bubble Tea event loop as a modal instead.
			// --dangerously-allow-all keeps its non-interactive AllowPrompter
			// and installs no modal.
			var prompter *tui.Prompter
			if allowAll {
				opts.Prompter = permission.AllowPrompter{}
			} else {
				prompter = tui.NewPrompter()
				opts.Prompter = prompter
			}

			session, err := build(opts)
			if err != nil {
				return err
			}

			return run(tui.New(tui.Deps{
				Loop:         session.loop,
				Model:        session.model,
				Prompter:     prompter,
				Models:       session.lister,
				SystemPrompt: session.prompt,
			}))
		},
	}

	cmd.Flags().StringVar(&opts.Model, "model", "", "override the configured model")
	cmd.Flags().StringVar(&opts.SystemPrompt, "system", "", "override the configured system prompt")
	cmd.Flags().BoolVar(&allowAll, "dangerously-allow-all", false, "auto-approve every tool call without prompting (unsafe)")

	return cmd
}

// defaultBuildTUI is the production tuiLoopBuilder: it loads config and
// credentials from disk, resolves the display model name, and builds the agent
// loop, the provider that backs the /model selector's listing, and the
// system-prompt rebuilder used when the model is switched live.
func defaultBuildTUI(opts agent.Options) (tuiSession, error) {
	loaded, err := config.Load()
	if err != nil {
		return tuiSession{}, fmt.Errorf("tui: load config: %w", err)
	}

	creds, err := auth.Load(auth.DefaultPath())
	if err != nil {
		return tuiSession{}, fmt.Errorf("tui: %w", err)
	}

	if opts.ProjectRoot == "" {
		opts.ProjectRoot = loaded.ProjectRoot
	}

	modelName, err := agent.ResolveModel(loaded.Config, creds, opts)
	if err != nil {
		return tuiSession{}, fmt.Errorf("tui: %w", err)
	}

	loop, err := agent.BuildLoop(loaded.Config, creds, opts)
	if err != nil {
		return tuiSession{}, fmt.Errorf("tui: %w", err)
	}

	lister, err := agent.BuildProvider(loaded.Config, creds, opts)
	if err != nil {
		return tuiSession{}, fmt.Errorf("tui: %w", err)
	}

	prompt := func(model string) (string, bool) {
		sp, err := agent.BuildSystemPrompt(loaded.Config, opts, model)
		if err != nil {
			return "", false
		}
		return sp, true
	}

	return tuiSession{loop: loop, lister: lister, prompt: prompt, model: modelName}, nil
}

// defaultRunTUI starts the Bubble Tea program on the alternate screen. Bubble
// Tea owns the terminal for the program's lifetime, so nothing else writes to
// stdout while it runs.
func defaultRunTUI(model tea.Model) error {
	// Mouse cell motion is enabled so the conversation view scrolls with the
	// wheel. It captures mouse events, so native click-drag selection is
	// replaced by the terminal's shift-drag (or equivalent) to copy text.
	program := tea.NewProgram(model, tea.WithAltScreen(), tea.WithMouseCellMotion())
	if _, err := program.Run(); err != nil {
		return fmt.Errorf("tui: %w", err)
	}
	return nil
}
