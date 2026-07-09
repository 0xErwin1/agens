package permission

import (
	"context"
	"fmt"
	"sync/atomic"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/bmatcuk/doublestar/v4"
)

// Mode is the operating mode enforced as a hard pre-check in Evaluate.
// ModeEdit preserves the ruleset's Allow/Ask/Deny behavior unchanged;
// ModeChat blocks every call the Engine's WriteClassifier flags as a write,
// before the ruleset or the Prompter are ever consulted.
type Mode int

const (
	// ModeEdit is the zero value: today's behavior, unaffected by the
	// chat-mode hard pre-check.
	ModeEdit Mode = iota
	ModeChat
)

// ModeState is a live-mutable, concurrency-safe holder for the current
// Mode, read once per Evaluate call so a surface (for example a TUI /mode
// command) can toggle it without rebuilding the Engine.
type ModeState struct {
	mode atomic.Int32
}

// NewModeState returns a ModeState initialized to initial.
func NewModeState(initial Mode) *ModeState {
	s := &ModeState{}
	s.mode.Store(int32(initial))
	return s
}

// Get returns the current Mode.
func (s *ModeState) Get() Mode {
	return Mode(s.mode.Load())
}

// Set updates the current Mode, visible to the next Evaluate call.
func (s *ModeState) Set(m Mode) {
	s.mode.Store(int32(m))
}

// WriteClassifier reports whether name identifies a tool the Engine must
// treat as a write when Mode is ModeChat.
type WriteClassifier func(name string) bool

// Engine resolves a tool call to a Decision. Two hard pre-checks run before
// the ruleset: an absolute global-deny match, then (when configured) a
// chat-mode write block. Only if neither hard check fires does Evaluate fall
// through to the static rules plus the Store's dynamic rules, last-match-wins.
type Engine struct {
	rules        []Rule
	globalDenies []Rule
	store        Store
	projector    Projector
	mode         func() Mode
	isWrite      WriteClassifier
}

// EngineOption configures an Engine at construction time.
type EngineOption func(*Engine)

// WithProjector overrides the default ProjectRaw argument projection.
func WithProjector(p Projector) EngineOption {
	return func(e *Engine) {
		e.projector = p
	}
}

// WithGlobalDenies installs rules as the Engine's absolute deny hard
// pre-check: a match here is returned as DecisionDeny before the ruleset or
// the Prompter are consulted, and no other rule source (static rules, a
// persisted Store rule, or a future bypass answerer) can loosen it, because
// these rules are never appended to the last-match-wins ruleset itself.
func WithGlobalDenies(rules []Rule) EngineOption {
	return func(e *Engine) {
		e.globalDenies = rules
	}
}

// WithMode installs mode as the live Mode reader for the chat-mode hard
// pre-check. mode is called once per Evaluate, so a caller backing it with
// a *ModeState can toggle the mode without rebuilding the Engine.
func WithMode(mode func() Mode) EngineOption {
	return func(e *Engine) {
		e.mode = mode
	}
}

// WithWriteClassifier installs classifier as the predicate the chat-mode
// hard pre-check uses to decide which tool names it blocks.
func WithWriteClassifier(classifier WriteClassifier) EngineOption {
	return func(e *Engine) {
		e.isWrite = classifier
	}
}

// NewEngine builds an Engine over rules plus whatever store later holds. It
// returns an error if any rule's Name or Argument — in rules or in a
// WithGlobalDenies option — is not a valid doublestar pattern. It panics if
// store is nil: that is a wiring bug, not a recoverable condition, mirroring
// Registry.Register and agentloop.New.
func NewEngine(rules []Rule, store Store, opts ...EngineOption) (*Engine, error) {
	if store == nil {
		panic("permission: NewEngine called with a nil Store")
	}

	e := &Engine{
		rules:     rules,
		store:     store,
		projector: ProjectRaw,
		mode:      func() Mode { return ModeEdit },
		isWrite:   func(string) bool { return false },
	}
	for _, opt := range opts {
		opt(e)
	}

	for _, r := range e.rules {
		if err := validateRule(r); err != nil {
			return nil, err
		}
	}
	for _, r := range e.globalDenies {
		if err := validateRule(r); err != nil {
			return nil, err
		}
	}

	return e, nil
}

// Evaluate resolves call to a Decision. It first checks the two hard
// pre-checks — a global-deny match, then a chat-mode write block — either of
// which short-circuits to DecisionDeny without consulting the ruleset or the
// Prompter. Otherwise it merges the Engine's static rules with the Store's
// current rules, in that order, and runs evaluate over the combined ruleset.
func (e *Engine) Evaluate(ctx context.Context, call message.ToolUsePart) (Decision, error) {
	arg := e.projector(call.Input)

	if evaluate(e.globalDenies, call.Name, arg) == DecisionDeny {
		return DecisionDeny, nil
	}

	if e.mode() == ModeChat && e.isWrite(call.Name) {
		return DecisionDeny, nil
	}

	stored, err := e.store.Rules(ctx)
	if err != nil {
		return DecisionAsk, fmt.Errorf("permission: read stored rules: %w", err)
	}

	rules := make([]Rule, 0, len(e.rules)+len(stored))
	rules = append(rules, e.rules...)
	rules = append(rules, stored...)

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

// RememberCall persists an argument-scoped Rule for decision: Name matches
// call.Name and Argument is the escaped, call-literal projection of
// call.Input, so a later call to the same tool with a different argument
// still resolves to Ask instead of inheriting this grant. When call.Input
// projects to an empty argument (an argument-less call), the persisted Rule
// falls back to name-scoped — the pre-argument-scoping Remember behavior —
// since there is no argument to scope on.
func (e *Engine) RememberCall(ctx context.Context, decision Decision, call message.ToolUsePart) error {
	arg := literalGlob(e.projector(call.Input))
	return e.Remember(ctx, Rule{Decision: decision, Name: call.Name, Argument: arg})
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
