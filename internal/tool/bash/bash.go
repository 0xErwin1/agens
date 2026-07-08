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
	"os"
	"os/exec"
	"strings"
	"syscall"
	"time"

	"github.com/0xErwin1/agens/internal/tool"
	"github.com/google/jsonschema-go/jsonschema"
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

	if ctx.Err() != nil {
		return tool.Result{}, ctx.Err()
	}
	if cmdCtx.Err() == context.DeadlineExceeded {
		return tool.Result{
			IsError: true,
			Text:    out.Text() + fmt.Sprintf("\nbash: command timed out after %s (process group killed)", d),
		}, nil
	}
	return mapRunResult(runErr, cmd, out), nil
}

// newCommand builds the exec.Cmd for command, rooted at b.projectRoot with
// out wired as both stdout and stderr.
//
// Setpgid makes bash the leader of its own process group (pgid == pid), so
// cmd.Cancel can SIGKILL the whole group via the negative pid instead of
// only the direct bash child. Without this, a backgrounded grandchild (e.g.
// "sleep 30 &") would survive a timeout or turn cancellation as an orphan:
// the default Cancel only kills cmd.Process. ESRCH means the group is
// already gone, which Cancel reports as os.ErrProcessDone rather than an
// error. cmd.WaitDelay bounds how long Wait will wait for a lingering
// descendant that still holds the output pipe open after bash itself has
// exited.
func (b *Bash) newCommand(cmdCtx context.Context, command string, out *cappedBuffer) *exec.Cmd {
	cmd := exec.CommandContext(cmdCtx, "bash", "-c", command)
	cmd.Dir = b.projectRoot
	cmd.Stdout = out
	cmd.Stderr = out
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error {
		err := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		if err == syscall.ESRCH {
			return os.ErrProcessDone
		}
		return err
	}
	cmd.WaitDelay = b.waitDelay
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

	if errors.Is(runErr, exec.ErrWaitDelay) {
		code := cmd.ProcessState.ExitCode()
		return tool.Result{
			IsError: code != 0,
			Text:    out.Text() + "\n(output may be incomplete: a background process kept the output stream open)",
		}
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
