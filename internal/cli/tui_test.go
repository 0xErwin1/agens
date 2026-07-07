package cli

import (
	"bytes"
	"errors"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/agent"
	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/permission"
)

// stubTUILoop builds a throwaway loop from a fake provider so the builder seam
// can return a real *agentloop.Loop without touching config, auth, or a
// network.
func stubTUILoop() *agentloop.Loop {
	return agentloop.New(&chatFakeProvider{steps: textDeltaSteps("ok")}, nil, agentloop.WithModel("gpt-test"))
}

func TestTUICommand_FlagsReachBuilderOptions(t *testing.T) {
	var received agent.Options
	build := func(opts agent.Options) (tuiSession, error) {
		received = opts
		return tuiSession{loop: stubTUILoop(), model: "gpt-test"}, nil
	}

	var ranModel tea.Model
	run := func(m tea.Model) error {
		ranModel = m
		return nil
	}

	cmd := newRootCommand(build, run)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"--model", "gpt-custom", "--system", "custom prompt", "--max-iterations", "17"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	if received.Model != "gpt-custom" {
		t.Fatalf("Options.Model = %q, want %q", received.Model, "gpt-custom")
	}
	if received.SystemPrompt != "custom prompt" {
		t.Fatalf("Options.SystemPrompt = %q, want %q", received.SystemPrompt, "custom prompt")
	}
	if received.MaxIterations != 17 {
		t.Fatalf("Options.MaxIterations = %d, want 17", received.MaxIterations)
	}
	if ranModel == nil {
		t.Fatal("run seam was not called with a model, want the constructed TUI model")
	}
}

func TestTUICommand_RejectsInvalidMaxIterationsFlag(t *testing.T) {
	for _, args := range [][]string{
		{"--max-iterations=0"},
		{"--max-iterations=-1"},
	} {
		t.Run(strings.Join(args, " "), func(t *testing.T) {
			builderCalled := false
			build := func(agent.Options) (tuiSession, error) {
				builderCalled = true
				return tuiSession{loop: stubTUILoop(), model: "gpt-test"}, nil
			}
			run := func(tea.Model) error { return nil }

			cmd := newRootCommand(build, run)
			cmd.SetOut(new(bytes.Buffer))
			cmd.SetErr(new(bytes.Buffer))
			cmd.SetArgs(args)

			err := cmd.Execute()
			if err == nil {
				t.Fatal("Execute() error = nil, want invalid max iterations error")
			}
			if !strings.Contains(err.Error(), "--max-iterations") {
				t.Fatalf("Execute() error = %q, want it to mention --max-iterations", err.Error())
			}
			if builderCalled {
				t.Fatal("builder was called, want validation to reject before building the loop")
			}
		})
	}
}

func TestTUICommand_DangerouslyAllowAllSelectsAllowPrompter(t *testing.T) {
	var received agent.Options
	build := func(opts agent.Options) (tuiSession, error) {
		received = opts
		return tuiSession{loop: stubTUILoop(), model: "gpt-test"}, nil
	}
	run := func(tea.Model) error { return nil }

	cmd := newRootCommand(build, run)
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
	build := func(opts agent.Options) (tuiSession, error) {
		received = opts
		return tuiSession{loop: stubTUILoop(), model: "gpt-test"}, nil
	}
	run := func(tea.Model) error { return nil }

	cmd := newRootCommand(build, run)
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
	build := func(agent.Options) (tuiSession, error) {
		return tuiSession{}, errors.New("tui: no credentials found")
	}

	ran := false
	run := func(tea.Model) error {
		ran = true
		return nil
	}

	cmd := newRootCommand(build, run)
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

func TestResolveResume(t *testing.T) {
	cases := []struct {
		name       string
		resume     bool
		args       []string
		wantID     string
		wantIsList bool
	}{
		{"bare agens starts fresh", false, nil, "", false},
		{"--resume with no id opens the list", true, nil, "", true},
		{"--resume with an id opens that session", true, []string{"abc"}, "abc", false},
		{"a positional id implies resume", false, []string{"abc"}, "abc", false},
		{"an empty positional falls back to the flag", true, []string{""}, "", true},
	}

	for _, c := range cases {
		id, openList := resolveResume(c.resume, c.args)
		if id != c.wantID || openList != c.wantIsList {
			t.Fatalf("%s: resolveResume(%v, %v) = (%q, %v), want (%q, %v)",
				c.name, c.resume, c.args, id, openList, c.wantID, c.wantIsList)
		}
	}
}

func TestRootCommand_BareInvocationRunsTUI(t *testing.T) {
	built := false
	build := func(agent.Options) (tuiSession, error) {
		built = true
		return tuiSession{loop: stubTUILoop(), model: "gpt-test"}, nil
	}

	var ranModel tea.Model
	run := func(m tea.Model) error {
		ranModel = m
		return nil
	}

	cmd := newRootCommand(build, run)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs(nil) // bare `agens`, no subcommand

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	if !built || ranModel == nil {
		t.Fatalf("bare agens did not build+run the TUI (built=%v, ranModel=%v)", built, ranModel)
	}
}

func TestRootCommand_KeepsSubcommandsAndDropsTUI(t *testing.T) {
	root := NewRootCommand()

	has := func(name string) bool {
		for _, c := range root.Commands() {
			if c.Name() == name {
				return true
			}
		}
		return false
	}

	for _, name := range []string{"auth", "config", "chat", "models"} {
		if !has(name) {
			t.Fatalf("root command no longer registers the %q subcommand", name)
		}
	}
	if has("tui") {
		t.Fatal("root command still registers a \"tui\" subcommand, want it removed in favor of bare agens")
	}
}
