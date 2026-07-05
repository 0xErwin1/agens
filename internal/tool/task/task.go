// Package task provides the "task" tool: it delegates a self-contained unit of
// work to a subagent that runs to completion with its own isolated context and
// returns its final report to the caller. The subagent is executed through an
// injected Runner, so this package depends on neither the agent loop nor a
// provider.
package task

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/google/jsonschema-go/jsonschema"

	"github.com/iperez/agens/internal/tool"
)

// Runner executes a subagent for a task description and returns its final text
// result. The composition root supplies an implementation backed by a nested
// agent loop with its own isolated conversation.
type Runner interface {
	Run(ctx context.Context, description string) (string, error)
}

// Tool is the "task" tool. It hands a description to a subagent through its
// Runner and returns whatever the subagent reports back.
type Tool struct {
	runner Runner
}

// New returns a task Tool backed by runner. It panics if runner is nil, since a
// task tool that cannot run a subagent is a wiring bug the composition root must
// fail fast on.
func New(runner Runner) *Tool {
	if runner == nil {
		panic("task: New called with a nil Runner")
	}
	return &Tool{runner: runner}
}

func (t *Tool) Name() string { return "task" }

func (t *Tool) Description() string {
	return "Delegate a self-contained task to a subagent that works on it with its " +
		"own isolated context and returns a final report. Use it to keep large, " +
		"separable pieces of work (deep research, a focused change) out of the main " +
		"conversation's context. The subagent runs to completion before you continue."
}

func (t *Tool) Schema() *jsonschema.Schema {
	return &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"description": {
				Type:        "string",
				Description: "the task for the subagent to carry out, stated as a complete, self-contained instruction it can act on without the surrounding conversation",
			},
		},
		Required: []string{"description"},
	}
}

type taskInput struct {
	Description string `json:"description"`
}

func (t *Tool) Execute(ctx context.Context, input json.RawMessage) (tool.Result, error) {
	var in taskInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("task: invalid input: %v", err)}, nil
	}
	if strings.TrimSpace(in.Description) == "" {
		return tool.Result{IsError: true, Text: "task: invalid input: description is required"}, nil
	}

	result, err := t.runner.Run(ctx, in.Description)
	if err != nil {
		// A canceled subagent surfaces as a Go error so the loop aborts the turn,
		// matching every other tool; any other failure is a tool-level error the
		// model can read and react to on its next turn.
		if ctx.Err() != nil {
			return tool.Result{}, err
		}
		return tool.Result{IsError: true, Text: "task: subagent failed: " + err.Error()}, nil
	}

	if strings.TrimSpace(result) == "" {
		return tool.Result{Text: "(the subagent returned no output)"}, nil
	}
	return tool.Result{Text: result}, nil
}
