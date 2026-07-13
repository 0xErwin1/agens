package permission

import (
	"context"
	"fmt"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/provider"
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
	bypass   func() bool
}

// GateOption configures optional Gate behavior.
type GateOption func(*Gate)

// WithBypass installs an optional reader for the current bypass state.
func WithBypass(enabled func() bool) GateOption {
	return func(g *Gate) {
		if enabled != nil {
			g.bypass = enabled
		}
	}
}

// NewGate returns a Gate wrapping inner. It panics if inner, engine, or
// prompter is nil: each is required for Run to behave meaningfully, a
// wiring bug rather than a recoverable condition.
func NewGate(inner Runner, engine *Engine, prompter Prompter, opts ...GateOption) *Gate {
	if inner == nil {
		panic("permission: NewGate called with a nil Runner")
	}
	if engine == nil {
		panic("permission: NewGate called with a nil Engine")
	}
	if prompter == nil {
		panic("permission: NewGate called with a nil Prompter")
	}
	gate := &Gate{
		inner:    inner,
		engine:   engine,
		prompter: prompter,
		bypass:   func() bool { return false },
	}
	for _, opt := range opts {
		if opt != nil {
			opt(gate)
		}
	}
	return gate
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
// the configured Prompter.
func (g *Gate) Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error) {
	allowed, err := g.authorize(ctx, call)
	if err != nil {
		return message.ToolResultPart{}, err
	}
	if !allowed {
		return deniedResult(call), nil
	}
	return g.inner.Run(ctx, call)
}

func (g *Gate) RunBatch(ctx context.Context, calls []message.ToolUsePart, onResult func(message.ToolResultPart)) ([]message.ToolResultPart, error) {
	preflight := make([]bool, len(calls))
	for i, call := range calls {
		allowed, err := g.authorize(ctx, call)
		if err != nil {
			return nil, err
		}
		preflight[i] = allowed
	}

	results := make([]message.ToolResultPart, 0, len(calls))
	for i, call := range calls {
		if err := ctx.Err(); err != nil {
			return nil, err
		}

		var result message.ToolResultPart
		if !preflight[i] {
			result = deniedResult(call)
		} else {
			var err error
			result, err = g.inner.Run(ctx, call)
			if err != nil {
				if ctx.Err() != nil {
					return nil, ctx.Err()
				}
				result = toolErrorResult(call, err)
			}
			result.ToolUseID = call.ID
		}

		results = append(results, result)
		if onResult != nil {
			onResult(result)
		}
	}
	return results, nil
}

func (g *Gate) authorize(ctx context.Context, call message.ToolUsePart) (bool, error) {
	if err := ctx.Err(); err != nil {
		return false, err
	}

	decision, err := g.engine.Evaluate(ctx, call)
	if err != nil {
		return false, err
	}

	switch decision {
	case DecisionAllow:
		return true, nil
	case DecisionDeny:
		return false, nil
	default:
		if g.bypass() {
			return true, nil
		}
		return g.resolveAsk(ctx, call)
	}
}

// resolveAsk consults the Prompter for a Decision that resolved to Ask.
// The "always" answers additionally remember an argument-scoped Rule (via
// RememberCall) before acting, so a later call to the same tool with the
// same argument resolves without prompting again, while a different
// argument still resolves to Ask. A Prompter error or an AnswerCancel answer
// returns a real error and never remembers a rule; if ctx was also
// canceled, ctx's error takes priority over the Prompter's own error.
func (g *Gate) resolveAsk(ctx context.Context, call message.ToolUsePart) (bool, error) {
	answer, err := g.prompter.Prompt(ctx, call)
	if err != nil {
		if ctx.Err() != nil {
			return false, ctx.Err()
		}
		return false, err
	}

	switch answer {
	case AnswerAllowOnce:
		return true, nil

	case AnswerAllowAlways:
		if err := g.engine.RememberCall(ctx, DecisionAllow, call); err != nil {
			return false, err
		}
		return true, nil

	case AnswerDenyAlways:
		if err := g.engine.RememberCall(ctx, DecisionDeny, call); err != nil {
			return false, err
		}
		return false, nil

	case AnswerCancel:
		return false, ErrCanceled

	default:
		return false, nil
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

func toolErrorResult(call message.ToolUsePart, err error) message.ToolResultPart {
	return message.ToolResultPart{
		ToolUseID: call.ID,
		Content:   message.Parts{message.TextPart{Text: err.Error()}},
		IsError:   true,
	}
}

var _ Runner = (*Gate)(nil)
