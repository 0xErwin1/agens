// Package task provides the "task" tool: it delegates a self-contained unit of
// work to a subagent that runs to completion with its own isolated context and
// returns its final report to the caller. The subagent is executed through an
// injected Runner, so this package depends on neither the agent loop nor a
// provider. The set of selectable subagents — and, per subagent, the models it
// may run on — is supplied as a mutable Catalog, so a surface can update what a
// delegation may pick mid-session, keeping this package free of any dependency
// on the agent-definition source.
package task

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"

	"github.com/google/jsonschema-go/jsonschema"

	"github.com/iperez/agens/internal/tool"
)

// Request is one delegation: the work to do, which subagent to run it on, and
// the model that subagent should use. Agent and Model may be empty, in which
// case the Runner applies its own defaults.
type Request struct {
	Description string
	Agent       string
	Model       string
}

// Runner executes a subagent for a Request and returns its final text result.
// The composition root supplies an implementation backed by a nested agent loop
// with its own isolated conversation.
type Runner interface {
	Run(ctx context.Context, req Request) (string, error)
}

// Agent describes one selectable subagent to the model: its name, a short
// description, and the models it is allowed to run on (empty means any served
// model is allowed).
type Agent struct {
	Name        string
	Description string
	Models      []string
}

// Tool is the "task" tool. It hands a Request to a subagent through its Runner
// and returns whatever the subagent reports back. The selectable subagents come
// from catalog, read on each schema build and each execution so an edit to the
// shared catalog takes effect on the next turn.
type Tool struct {
	runner  Runner
	catalog *Catalog
}

// New returns a task Tool backed by runner and offering the subagents of
// catalog. A nil catalog offers no selectable subagents (the tool takes only a
// description). It panics if runner is nil, since a task tool that cannot run a
// subagent is a wiring bug the composition root must fail fast on.
func New(runner Runner, catalog *Catalog) *Tool {
	if runner == nil {
		panic("task: New called with a nil Runner")
	}

	return &Tool{runner: runner, catalog: catalog}
}

// agents returns the current selectable subagents, or nil when no catalog is
// wired.
func (t *Tool) agents() []Agent {
	if t.catalog == nil {
		return nil
	}
	return t.catalog.Agents()
}

func (t *Tool) Name() string { return "task" }

func (t *Tool) Description() string {
	base := "Delegate a self-contained task to a subagent that works on it with its " +
		"own isolated context and returns a final report. Use it to keep large, " +
		"separable pieces of work (deep research, a focused change) out of the main " +
		"conversation's context. The subagent runs to completion before you continue."

	if len(t.agents()) == 0 {
		return base
	}

	return base + " Choose the subagent with the `agent` parameter; when the user asks for a " +
		"specific model — or a cheaper or faster one — for the delegated work, set `model` to it."
}

func (t *Tool) Schema() *jsonschema.Schema {
	props := map[string]*jsonschema.Schema{
		"description": {
			Type:        "string",
			Description: "the task for the subagent to carry out, stated as a complete, self-contained instruction it can act on without the surrounding conversation",
		},
	}

	if agents := t.agents(); len(agents) > 0 {
		props["agent"] = &jsonschema.Schema{
			Type:        "string",
			Enum:        agentEnum(agents),
			Description: agentParamDescription(agents),
		}
		props["model"] = &jsonschema.Schema{
			Type: "string",
			Description: "the model the subagent should run on. Omit to use the agent's default model. " +
				"When set, it must be one of the chosen agent's allowed models (see the agent parameter).",
		}
	}

	return &jsonschema.Schema{
		Type:       "object",
		Properties: props,
		Required:   []string{"description"},
	}
}

// agentEnum lists the selectable agent names for the schema's enum constraint.
func agentEnum(agents []Agent) []any {
	names := make([]any, 0, len(agents))
	for _, a := range agents {
		names = append(names, a.Name)
	}
	return names
}

// agentParamDescription documents each selectable agent and its allowed models
// so the model can pick an agent, and a valid model for it, from one place.
func agentParamDescription(agents []Agent) string {
	var b strings.Builder
	b.WriteString("which subagent to run. One of: ")
	for i, a := range agents {
		if i > 0 {
			b.WriteString("; ")
		}
		b.WriteString(a.Name)
		if a.Description != "" {
			b.WriteString(" — ")
			b.WriteString(a.Description)
		}
		if len(a.Models) > 0 {
			b.WriteString(" [models: ")
			b.WriteString(strings.Join(a.Models, ", "))
			b.WriteString("]")
		} else {
			b.WriteString(" [any model]")
		}
	}
	b.WriteString(". Defaults to ")
	b.WriteString(agents[0].Name)
	b.WriteString(" when omitted.")
	return b.String()
}

type taskInput struct {
	Description string `json:"description"`
	Agent       string `json:"agent"`
	Model       string `json:"model"`
}

func (t *Tool) Execute(ctx context.Context, input json.RawMessage) (tool.Result, error) {
	var in taskInput
	if err := json.Unmarshal(input, &in); err != nil {
		return tool.Result{IsError: true, Text: fmt.Sprintf("task: invalid input: %v", err)}, nil
	}
	if strings.TrimSpace(in.Description) == "" {
		return tool.Result{IsError: true, Text: "task: invalid input: description is required"}, nil
	}

	req, toolErr := t.resolveRequest(in)
	if toolErr != "" {
		return tool.Result{IsError: true, Text: toolErr}, nil
	}

	result, err := t.runner.Run(ctx, req)
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

// resolveRequest validates the requested agent and model against the offered
// agents and returns the Request to run. When no agents are offered, the raw
// input is passed through unchanged. It returns a non-empty tool-error message
// (to surface to the model) when the agent is unknown or the model is not
// allowed for the chosen agent.
func (t *Tool) resolveRequest(in taskInput) (Request, string) {
	req := Request(in)

	agents := t.agents()
	if len(agents) == 0 {
		return req, ""
	}

	byName := make(map[string]Agent, len(agents))
	for _, a := range agents {
		byName[a.Name] = a
	}

	name := in.Agent
	if name == "" {
		name = agents[0].Name
	}

	agent, ok := byName[name]
	if !ok {
		return Request{}, fmt.Sprintf("task: unknown agent %q; available agents: %s", in.Agent, agentNames(agents))
	}
	req.Agent = name

	if in.Model != "" && len(agent.Models) > 0 && !allows(agent.Models, in.Model) {
		return Request{}, fmt.Sprintf("task: agent %q cannot run model %q; allowed models: %s", name, in.Model, strings.Join(agent.Models, ", "))
	}

	return req, ""
}

func agentNames(agents []Agent) string {
	names := make([]string, 0, len(agents))
	for _, a := range agents {
		names = append(names, a.Name)
	}
	return strings.Join(names, ", ")
}

func allows(models []string, model string) bool {
	for _, m := range models {
		if m == model {
			return true
		}
	}
	return false
}
