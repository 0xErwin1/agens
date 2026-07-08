package permission

import (
	"context"
	"fmt"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/bmatcuk/doublestar/v4"
)

// Engine resolves a tool call to a Decision by evaluating a fixed set of
// static rules followed by a Store's dynamic rules, last-match-wins.
type Engine struct {
	rules     []Rule
	store     Store
	projector Projector
}

// EngineOption configures an Engine at construction time.
type EngineOption func(*Engine)

// WithProjector overrides the default ProjectRaw argument projection.
func WithProjector(p Projector) EngineOption {
	return func(e *Engine) {
		e.projector = p
	}
}

// NewEngine builds an Engine over rules plus whatever store later holds.
// It returns an error if any rule's Name or Argument is not a valid
// doublestar pattern. It panics if store is nil: that is a wiring bug, not
// a recoverable condition, mirroring Registry.Register and agentloop.New.
func NewEngine(rules []Rule, store Store, opts ...EngineOption) (*Engine, error) {
	if store == nil {
		panic("permission: NewEngine called with a nil Store")
	}

	for _, r := range rules {
		if err := validateRule(r); err != nil {
			return nil, err
		}
	}

	e := &Engine{
		rules:     rules,
		store:     store,
		projector: ProjectRaw,
	}
	for _, opt := range opts {
		opt(e)
	}

	return e, nil
}

// Evaluate resolves call to a Decision by merging the Engine's static rules
// with the Store's current rules, in that order, and running evaluate over
// the combined ruleset.
func (e *Engine) Evaluate(ctx context.Context, call message.ToolUsePart) (Decision, error) {
	stored, err := e.store.Rules(ctx)
	if err != nil {
		return DecisionAsk, fmt.Errorf("permission: read stored rules: %w", err)
	}

	rules := make([]Rule, 0, len(e.rules)+len(stored))
	rules = append(rules, e.rules...)
	rules = append(rules, stored...)

	arg := e.projector(call.Input)
	return evaluate(rules, call.Name, arg), nil
}

// Remember validates r and appends it to the Store, so it takes part in
// every subsequent Evaluate call as the last-checked rule.
func (e *Engine) Remember(ctx context.Context, r Rule) error {
	if err := validateRule(r); err != nil {
		return err
	}
	return e.store.Append(ctx, r)
}

// validateRule reports an error if r.Name or r.Argument is not a valid
// doublestar pattern, so every rule reaching Evaluate is guaranteed to
// match without error.
func validateRule(r Rule) error {
	if !doublestar.ValidatePattern(r.Name) {
		return fmt.Errorf("permission: invalid name pattern %q", r.Name)
	}
	if r.Argument != "" && !doublestar.ValidatePattern(r.Argument) {
		return fmt.Errorf("permission: invalid argument pattern %q", r.Argument)
	}
	return nil
}
