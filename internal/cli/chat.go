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

	"github.com/iperez/agens/internal/agent"
	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
	"github.com/iperez/agens/internal/message"
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

			ctx, stop := signal.NotifyContext(cmd.Context(), os.Interrupt, syscall.SIGTERM)
			defer stop()

			opts.Prompter = selectPrompter(allowAll)

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

	return cmd
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

	loop, err := agent.BuildLoop(loaded.Config, creds, opts)
	if err != nil {
		return nil, fmt.Errorf("chat: %w", err)
	}
	return loop, nil
}
