package cli

import (
	"errors"
	"fmt"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/google/uuid"
	"github.com/spf13/cobra"

	"github.com/iperez/agens/internal/agent"
	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/permission"
	"github.com/iperez/agens/internal/session"
	"github.com/iperez/agens/internal/tui"
)

// tuiSession is everything the TUI needs from the build step: the agent loop,
// the model lister for the /model selector, the system-prompt rebuilder used
// on a live model switch, and the model name for the status line.
type tuiSession struct {
	loop         *agentloop.Loop
	lister       tui.ModelLister
	prompt       tui.SystemPromptFunc
	model        string
	effortLevels []string
	sessions     tui.SessionStore
	files        tui.FileSource
	project      string

	collapseThinking   bool
	truncateToolOutput bool
}

// tuiLoopBuilder resolves an agent.Options into a tuiSession. It is the
// dependency-injection seam that lets tests exercise the command against a
// fake provider without touching config, auth, or the network.
type tuiLoopBuilder func(opts agent.Options) (tuiSession, error)

// tuiRunner starts the interactive program for the given root model. It is a
// second seam, distinct from the builder, so tests can verify the command
// wiring without starting a Bubble Tea program (which requires a TTY).
type tuiRunner func(model tea.Model) error

// configureRootTUI wires the interactive terminal UI onto cmd as its default
// action: running agens with no subcommand builds the agent session and starts
// the TUI. The build and run seams are injected so tests can exercise the
// wiring without config, auth, a network, or a TTY.
func configureRootTUI(cmd *cobra.Command, build tuiLoopBuilder, run tuiRunner) {
	var opts agent.Options
	var allowAll bool
	var resume bool

	cmd.RunE = func(cmd *cobra.Command, args []string) error {
		if cmd.Flags().Changed("max-iterations") && opts.MaxIterations < 1 {
			return errors.New("tui: --max-iterations must be >= 1")
		}

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

		sess, err := build(opts)
		if err != nil {
			return err
		}

		resumeID, openSessions := resolveResume(resume, args)

		return run(tui.New(tui.Deps{
			Loop:               sess.loop,
			Model:              sess.model,
			Prompter:           prompter,
			Models:             sess.lister,
			SystemPrompt:       sess.prompt,
			EffortLevels:       sess.effortLevels,
			Sessions:           sess.sessions,
			NewSessionID:       uuid.NewString,
			Files:              sess.files,
			Project:            sess.project,
			ResumeID:           resumeID,
			OpenSessions:       openSessions,
			CollapseThinking:   sess.collapseThinking,
			TruncateToolOutput: sess.truncateToolOutput,
		}))
	}

	cmd.Flags().StringVar(&opts.Model, "model", "", "override the configured model")
	cmd.Flags().StringVar(&opts.SystemPrompt, "system", "", "override the configured system prompt")
	cmd.Flags().IntVar(&opts.MaxIterations, "max-iterations", 0, "override the configured agent loop iteration limit")
	cmd.Flags().BoolVar(&allowAll, "dangerously-allow-all", false, "auto-approve every tool call without prompting (unsafe)")
	cmd.Flags().BoolVar(&resume, "resume", false, "resume a saved conversation: pass a session id to open it, or omit the id to pick from the list")
}

// resolveResume maps the --resume flag and an optional positional session id to
// the TUI's startup behavior: a non-empty id resumes that session directly; a
// bare --resume opens the session picker; neither starts a fresh conversation.
// A positional id implies resume even without the flag.
func resolveResume(resume bool, args []string) (resumeID string, openSessions bool) {
	if len(args) > 0 && args[0] != "" {
		return args[0], false
	}
	return "", resume
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

	prov, err := agent.BuildProvider(loaded.Config, creds, opts)
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

	// @-references are best-effort: a project root that cannot be opened as a
	// confinement root simply disables them rather than failing the TUI.
	var files tui.FileSource
	if src, err := newProjectFileSource(opts.ProjectRoot); err == nil {
		files = src
	}

	return tuiSession{
		loop:               loop,
		lister:             prov,
		prompt:             prompt,
		model:              modelName,
		effortLevels:       prov.EffortLevels(),
		sessions:           session.NewStore(session.DefaultDir()),
		files:              files,
		project:            opts.ProjectRoot,
		collapseThinking:   loaded.Config.UI.CollapseThinking,
		truncateToolOutput: loaded.Config.UI.TruncateToolOutput,
	}, nil
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
