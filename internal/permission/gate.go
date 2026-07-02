package permission

import (
	"context"
	"fmt"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// Runner mirrors agentloop.ToolRunner so Gate can decorate an inner tool
// runner without internal/permission importing internal/agentloop.
type Runner interface {
	Specs() []provider.ToolSpec
	Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error)
}

// Gate decorates an inner Runner with an Allow|Ask|Deny permission check,
// resolved by an Engine, before every call executes.
type Gate struct {
	inner    Runner
	engine   *Engine
	prompter Prompter
}

// NewGate returns a Gate wrapping inner. It panics if inner, engine, or
// prompter is nil: each is required for Run to behave meaningfully, a
// wiring bug rather than a recoverable condition.
func NewGate(inner Runner, engine *Engine, prompter Prompter) *Gate {
	if inner == nil {
		panic("permission: NewGate called with a nil Runner")
	}
	if engine == nil {
		panic("permission: NewGate called with a nil Engine")
	}
	if prompter == nil {
		panic("permission: NewGate called with a nil Prompter")
	}
	return &Gate{inner: inner, engine: engine, prompter: prompter}
}

// Specs returns the inner Runner's Specs() unchanged: permissions never
// hide or reorder the tools offered to the model.
func (g *Gate) Specs() []provider.ToolSpec {
	return g.inner.Specs()
}

// Run resolves call to an Allow|Ask|Deny Decision before dispatch.
//
// ctx.Err() is checked before evaluation, mirroring Registry.Run's
// pre-dispatch check. Allow delegates to the inner Runner unchanged. Deny
// returns a denied ToolResultPart with a nil Go error, mirroring
// Registry.Run's unknown-tool path: a denial is a tool-level failure the
// model can see, not a Go error that aborts the turn. Ask is resolved by
// runAsk.
func (g *Gate) Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error) {
	if err := ctx.Err(); err != nil {
		return message.ToolResultPart{}, err
	}

	decision, err := g.engine.Evaluate(ctx, call)
	if err != nil {
		return message.ToolResultPart{}, err
	}

	switch decision {
	case DecisionAllow:
		return g.inner.Run(ctx, call)
	case DecisionDeny:
		return deniedResult(call), nil
	default:
		return g.runAsk(ctx, call)
	}
}

// runAsk consults the Prompter for a Decision that resolved to Ask.
// allow-once/allow-always execute the call; deny-once and any unrecognized
// Answer deny it. The "always" answers additionally remember a name-only
// Rule before acting, so a later matching call resolves without prompting
// again. A Prompter error or an AnswerCancel answer returns a real error
// and never remembers a rule; if ctx was also canceled, ctx's error takes
// priority over the Prompter's own error.
func (g *Gate) runAsk(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error) {
	answer, err := g.prompter.Prompt(ctx, call)
	if err != nil {
		if ctx.Err() != nil {
			return message.ToolResultPart{}, ctx.Err()
		}
		return message.ToolResultPart{}, err
	}

	switch answer {
	case AnswerAllowOnce:
		return g.inner.Run(ctx, call)

	case AnswerAllowAlways:
		if err := g.engine.Remember(ctx, Rule{Decision: DecisionAllow, Name: call.Name}); err != nil {
			return message.ToolResultPart{}, err
		}
		return g.inner.Run(ctx, call)

	case AnswerDenyAlways:
		if err := g.engine.Remember(ctx, Rule{Decision: DecisionDeny, Name: call.Name}); err != nil {
			return message.ToolResultPart{}, err
		}
		return deniedResult(call), nil

	case AnswerCancel:
		return message.ToolResultPart{}, ErrCanceled

	default:
		return deniedResult(call), nil
	}
}

// deniedResult is the ToolResultPart returned for a denied call: the exact
// shape of Registry.Run's unknown-tool path, so the model sees a normal
// tool-level failure rather than an aborted turn.
func deniedResult(call message.ToolUsePart) message.ToolResultPart {
	return message.ToolResultPart{
		ToolUseID: call.ID,
		Content: message.Parts{message.TextPart{
			Text: fmt.Sprintf("permission denied: tool %q was not executed", call.Name),
		}},
		IsError: true,
	}
}

var _ Runner = (*Gate)(nil)
