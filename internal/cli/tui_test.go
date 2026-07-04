package cli

import (
	"bytes"
	"errors"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/agent"
	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/permission"
	"github.com/iperez/agens/internal/tui"
)

// stubTUILoop builds a throwaway loop from a fake provider so the builder seam
// can return a real *agentloop.Loop without touching config, auth, or a
// network.
func stubTUILoop() *agentloop.Loop {
	return agentloop.New(&chatFakeProvider{steps: textDeltaSteps("ok")}, nil, agentloop.WithModel("gpt-test"))
}

func TestTUICommand_FlagsReachBuilderOptions(t *testing.T) {
	var received agent.Options
	build := func(opts agent.Options) (*agentloop.Loop, tui.ModelLister, string, error) {
		received = opts
		return stubTUILoop(), nil, "gpt-test", nil
	}

	var ranModel tea.Model
	run := func(m tea.Model) error {
		ranModel = m
		return nil
	}

	cmd := newTUICommandWithBuilder(build, run)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"--model", "gpt-custom", "--system", "custom prompt"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	if received.Model != "gpt-custom" {
		t.Fatalf("Options.Model = %q, want %q", received.Model, "gpt-custom")
	}
	if received.SystemPrompt != "custom prompt" {
		t.Fatalf("Options.SystemPrompt = %q, want %q", received.SystemPrompt, "custom prompt")
	}
	if ranModel == nil {
		t.Fatal("run seam was not called with a model, want the constructed TUI model")
	}
}

func TestTUICommand_DangerouslyAllowAllSelectsAllowPrompter(t *testing.T) {
	var received agent.Options
	build := func(opts agent.Options) (*agentloop.Loop, tui.ModelLister, string, error) {
		received = opts
		return stubTUILoop(), nil, "gpt-test", nil
	}
	run := func(tea.Model) error { return nil }

	cmd := newTUICommandWithBuilder(build, run)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"--dangerously-allow-all"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	if _, ok := received.Prompter.(permission.AllowPrompter); !ok {
		t.Fatalf("Options.Prompter = %T, want permission.AllowPrompter", received.Prompter)
	}
}

func TestTUICommand_DefaultPrompterIsNotAllowPrompter(t *testing.T) {
	var received agent.Options
	build := func(opts agent.Options) (*agentloop.Loop, tui.ModelLister, string, error) {
		received = opts
		return stubTUILoop(), nil, "gpt-test", nil
	}
	run := func(tea.Model) error { return nil }

	cmd := newTUICommandWithBuilder(build, run)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs(nil)

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	if _, ok := received.Prompter.(permission.AllowPrompter); ok {
		t.Fatal("Options.Prompter = permission.AllowPrompter, want it unset without --dangerously-allow-all")
	}
}

func TestTUICommand_BuilderErrorPropagatesAndSkipsRun(t *testing.T) {
	build := func(agent.Options) (*agentloop.Loop, tui.ModelLister, string, error) {
		return nil, nil, "", errors.New("tui: no credentials found")
	}

	ran := false
	run := func(tea.Model) error {
		ran = true
		return nil
	}

	cmd := newTUICommandWithBuilder(build, run)
	out := new(bytes.Buffer)
	errOut := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(errOut)
	cmd.SetArgs(nil)

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want the builder error to propagate")
	}
	if !strings.Contains(err.Error(), "no credentials") {
		t.Fatalf("Execute() error = %q, want it to carry the builder error", err.Error())
	}
	if ran {
		t.Fatal("run seam was called even though the builder failed, want it skipped")
	}
}

func TestRootCommand_RegistersTUI(t *testing.T) {
	root := NewRootCommand()

	found := false
	for _, c := range root.Commands() {
		if c.Name() == "tui" {
			found = true
			break
		}
	}
	if !found {
		t.Fatal("root command does not register a \"tui\" subcommand")
	}
}
