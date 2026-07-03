// Package bash implements the "bash" tool: it runs a shell command via
// bash -c from the project root, capturing combined stdout/stderr up to a
// fixed cap and reporting the exit status. It is NOT sandboxed — the Ask
// permission prompt is the only boundary between the model and the host
// shell.
package bash

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os/exec"
	"strings"
	"time"

	"github.com/google/jsonschema-go/jsonschema"
	"github.com/iperez/agens/internal/tool"
)

const (
	defaultTimeout = 120 * time.Second
	waitDelay      = 2 * time.Second
	maxOutputBytes = 100 << 10
)

// Bash implements the "bash" tool.
type Bash struct {
	projectRoot    string
	defaultTimeout time.Duration
	waitDelay      time.Duration
}

// New returns a Bash tool rooted at projectRoot. It panics if projectRoot is
// empty, since that is a wiring bug the composition root must fail fast on.
func New(projectRoot string) *Bash {
	if projectRoot == "" {
		panic("bash: New called with an empty projectRoot")
	}
	return &Bash{
		projectRoot:    projectRoot,
		defaultTimeout: defaultTimeout,
		waitDelay:      waitDelay,
	}
}

func (b *Bash) Name() string { return "bash" }

func (b *Bash) Description() string {
	return "Run a shell command via bash -c from the project root. Captures combined stdout " +
		"and stderr and reports the exit status. Defaults to a 120s timeout. This tool is NOT " +
		"sandboxed: it runs with the same permissions and environment as the agent process, " +
		"and the Ask permission prompt is the only safety boundary."
}

func (b *Bash) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"command":         {Type: "string", Description: "the shell command to run via bash -c"},
			"timeout_seconds": {Type: "integer", Description: "max seconds before the command is killed (default: 120)"},
		},
		Required: []string{"command"},
	}
}

// bashInput is the schema of Bash's Execute input.
type bashInput struct {
	Command        string `json:"command"`
	TimeoutSeconds int    `json:"timeout_seconds"`
}

func (b *Bash) Execute(ctx context.Context, input json.RawMessage) (tool.Result, error) {
	var in bashInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("bash: invalid input: %v", err)}, nil
	}
	if strings.TrimSpace(in.Command) == "" {
		return tool.Result{IsError: true, Text: "bash: invalid input: command is required"}, nil
	}
	if in.TimeoutSeconds < 0 {
		return tool.Result{IsError: true, Text: "bash: invalid input: timeout_seconds must not be negative"}, nil
	}

	d := b.defaultTimeout
	if in.TimeoutSeconds > 0 {
		d = time.Duration(in.TimeoutSeconds) * time.Second
	}

	cmdCtx, cancel := context.WithTimeout(ctx, d)
	defer cancel()

	out := newCappedBuffer(maxOutputBytes)
	cmd := b.newCommand(cmdCtx, in.Command, out)

	runErr := cmd.Run()

	return mapRunResult(runErr, cmd, out), nil
}

// newCommand builds the exec.Cmd for command, rooted at b.projectRoot with
// out wired as both stdout and stderr.
func (b *Bash) newCommand(cmdCtx context.Context, command string, out *cappedBuffer) *exec.Cmd {
	cmd := exec.CommandContext(cmdCtx, "bash", "-c", command)
	cmd.Dir = b.projectRoot
	cmd.Stdout = out
	cmd.Stderr = out
	return cmd
}

// mapRunResult maps the outcome of cmd.Run() onto a tool.Result.
func mapRunResult(runErr error, cmd *exec.Cmd, out *cappedBuffer) tool.Result {
	if runErr == nil {
		text := out.Text()
		if text == "" {
			text = "(no output; exit status 0)"
		}
		return tool.Result{Text: text}
	}

	var exitErr *exec.ExitError
	if errors.As(runErr, &exitErr) {
		return tool.Result{
			IsError: true,
			Text:    out.Text() + "\n" + exitErr.String(),
		}
	}

	return tool.Result{IsError: true, Text: fmt.Sprintf("bash: failed to start: %v", runErr)}
}
