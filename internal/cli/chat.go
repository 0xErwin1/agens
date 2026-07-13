package cli

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"strings"
	"syscall"

	"github.com/spf13/cobra"

	"github.com/0xErwin1/agens/internal/agent"
	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/auth"
	"github.com/0xErwin1/agens/internal/config"
	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/permission"
	"github.com/0xErwin1/agens/internal/permission/permissiondb"
)

// loopBuilder resolves an agent.Options into a ready-to-run
// *agentloop.Loop. It is the dependency-injection seam that lets tests
// exercise the chat command against a fake provider without touching
// config, auth, or the network.
type loopBuilder func(opts agent.Options) (*agentloop.Loop, error)

func newChatCommand() *cobra.Command {
	return newChatCommandWithBuilder(defaultBuildLoop)
}

func newChatCommandWithBuilder(build loopBuilder) *cobra.Command {
	var opts agent.Options
	var allowAll bool
	var modeFlag string

	cmd := &cobra.Command{
		Use:   "chat [prompt]",
		Short: "Send a one-shot prompt to the configured agent",
		Long:  "Send a one-shot prompt to the configured agent and stream its response to stdout.\nThe prompt is read from the positional argument if given, otherwise from stdin.",
		Args:  cobra.MaximumNArgs(1),
		RunE: func(cmd *cobra.Command, args []string) error {
			prompt, err := resolvePrompt(cmd, args)
			if err != nil {
				return err
			}
			if cmd.Flags().Changed("max-iterations") && opts.MaxIterations < 1 {
				return errors.New("chat: --max-iterations must be >= 1")
			}

			mode, err := parseModeFlag(modeFlag)
			if err != nil {
				return fmt.Errorf("chat: %w", err)
			}
			opts.Mode = permission.NewModeState(mode)
			opts.Bypass = permission.NewBypassState(allowAll)

			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()

			opts.Prompter = selectPrompter(false)

			// Surface any skipped agent-definition or skill files to stderr so a
			// malformed file is visible rather than silently dropped; neither blocks
			// the turn.
			if _, warnings, derr := agent.LoadAgentDefs(opts); derr == nil {
				for _, w := range warnings {
					_, _ = fmt.Fprintln(cmd.ErrOrStderr(), "warning: "+w)
				}
			}
			if _, warnings, derr := agent.LoadSkills(opts); derr == nil {
				for _, w := range warnings {
					_, _ = fmt.Fprintln(cmd.ErrOrStderr(), "warning: "+w)
				}
			}

			loop, err := build(opts)
			if err != nil {
				return err
			}

			history := []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: prompt})}

			printed := false
			var writeErr error
			sink := func(ev agentloop.LoopEvent) {
				if ev.Kind != agentloop.LoopTextDelta {
					return
				}
				if _, werr := fmt.Fprint(cmd.OutOrStdout(), ev.Text); werr != nil {
					writeErr = werr
					return
				}
				printed = true
			}

			_, runErr := loop.Run(ctx, history, sink)
			if printed && writeErr == nil {
				if _, werr := fmt.Fprintln(cmd.OutOrStdout()); werr != nil {
					writeErr = werr
				}
			}
			return errors.Join(runErr, writeErr)
		},
	}

	cmd.Flags().StringVar(&opts.Model, "model", "", "override the configured model")
	cmd.Flags().StringVar(&opts.SystemPrompt, "system", "", "override the configured system prompt")
	cmd.Flags().IntVar(&opts.MaxIterations, "max-iterations", 0, "override the configured agent loop iteration limit")
	cmd.Flags().BoolVar(&allowAll, "dangerously-allow-all", false, "auto-approve every tool call without prompting (unsafe)")
	cmd.Flags().StringVar(&modeFlag, "mode", "edit", `starting operating mode: "edit" (default) or "chat" (blocks all writes and bash)`)

	return cmd
}

// parseModeFlag resolves the --mode flag's value into a permission.Mode,
// case-insensitively and trimmed. It is the CLI-flag counterpart of the TUI's
// /mode command: unlike the command, a flag has no blank-toggle case, since
// there is no live session yet to toggle relative to.
func parseModeFlag(value string) (permission.Mode, error) {
	switch strings.ToLower(strings.TrimSpace(value)) {
	case "edit", "":
		return permission.ModeEdit, nil
	case "chat":
		return permission.ModeChat, nil
	default:
		return 0, fmt.Errorf("--mode must be \"chat\" or \"edit\", got %q", value)
	}
}

// resolvePrompt returns the prompt for one chat turn: the sole positional
// argument if non-blank, otherwise the trimmed contents of stdin.
func resolvePrompt(cmd *cobra.Command, args []string) (string, error) {
	if len(args) == 1 {
		if trimmed := strings.TrimSpace(args[0]); trimmed != "" {
			return trimmed, nil
		}
	}

	data, err := io.ReadAll(cmd.InOrStdin())
	if err != nil {
		return "", fmt.Errorf("chat: read stdin: %w", err)
	}

	trimmed := strings.TrimSpace(string(data))
	if trimmed == "" {
		return "", errors.New("chat: a prompt is required (argument or stdin)")
	}
	return trimmed, nil
}

// defaultBuildLoop is the production loopBuilder: it loads config and
// credentials from disk before delegating to agent.BuildLoop.
//
// It also opens the project-scoped permissiondb.Store backing
// opts.PermissionStore, so a one-shot chat run honors and can extend the same
// persisted allow/deny-always grants an interactive session builds up. The
// store is deliberately left open for the process's lifetime rather than
// deferred-closed here: this function returns well before the loop actually
// runs, and a one-shot command process exits shortly after RunE returns,
// reclaiming the handle the same way sessiondb's already does.
func defaultBuildLoop(opts agent.Options) (*agentloop.Loop, error) {
	loaded, err := config.Load()
	if err != nil {
		return nil, fmt.Errorf("chat: load config: %w", err)
	}

	creds, err := auth.Load(auth.DefaultPath())
	if err != nil {
		return nil, fmt.Errorf("chat: %w", err)
	}

	if opts.ProjectRoot == "" {
		opts.ProjectRoot = loaded.ProjectRoot
	}

	store, err := permissiondb.Open(permissiondb.DefaultPath(), opts.ProjectRoot)
	if err != nil {
		return nil, fmt.Errorf("chat: open permissions: %w", err)
	}
	opts.PermissionStore = store

	loop, err := agent.BuildLoop(loaded.Config, creds, opts)
	if err != nil {
		return nil, fmt.Errorf("chat: %w", err)
	}
	return loop, nil
}
