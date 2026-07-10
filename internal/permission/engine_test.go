package permission

import (
	"context"
	"encoding/json"
	"errors"
	"testing"

	"github.com/0xErwin1/agens/internal/message"
)

// fakeStore is a Store double: canned Rules/Append results and a record of
// every appended Rule.
type fakeStore struct {
	rules     []Rule
	rulesErr  error
	appendErr error
	appended  []Rule
}

func (f *fakeStore) Rules(ctx context.Context) ([]Rule, error) {
	if f.rulesErr != nil {
		return nil, f.rulesErr
	}
	return f.rules, nil
}

func (f *fakeStore) Append(ctx context.Context, r Rule) error {
	if f.appendErr != nil {
		return f.appendErr
	}
	f.appended = append(f.appended, r)
	return nil
}

var _ Store = (*fakeStore)(nil)

func TestNewEngine_PanicsOnNilStore(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatalf("NewEngine(nil store) did not panic, want a panic")
		}
	}()
	_, _ = NewEngine(nil, nil)
}

func TestNewEngine_ErrorsOnInvalidGlob(t *testing.T) {
	_, err := NewEngine([]Rule{{Decision: DecisionAllow, Name: "["}}, &fakeStore{})
	if err == nil {
		t.Fatalf("NewEngine() with an invalid glob pattern error = nil, want non-nil")
	}
}

func TestNewEngine_ErrorsOnInvalidArgumentGlob(t *testing.T) {
	_, err := NewEngine([]Rule{{Decision: DecisionAllow, Name: "bash", Argument: "["}}, &fakeStore{})
	if err == nil {
		t.Fatalf("NewEngine() with an invalid argument glob pattern error = nil, want non-nil")
	}
}

func TestEngine_Evaluate_MergesStaticAndStoredRules(t *testing.T) {
	store := &fakeStore{rules: []Rule{{Decision: DecisionDeny, Name: "bash"}}}
	engine, err := NewEngine([]Rule{{Decision: DecisionAllow, Name: "*"}}, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{Name: "bash"})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionDeny {
		t.Fatalf("Evaluate() = %v, want %v (stored rule evaluated after static rules)", got, DecisionDeny)
	}
}

func TestEngine_Evaluate_PropagatesStoreReadError(t *testing.T) {
	store := &fakeStore{rulesErr: errors.New("boom")}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	_, err = engine.Evaluate(context.Background(), message.ToolUsePart{Name: "bash"})
	if err == nil {
		t.Fatalf("Evaluate() error = nil, want non-nil when Store.Rules fails")
	}
}

func TestEngine_Evaluate_DefaultProjectorUsesRawInput(t *testing.T) {
	// The argument glob avoids literal `{`/`}`: doublestar treats unescaped
	// braces as an alternation group, so a pattern equal to raw JSON input
	// (which starts with `{`) would need escaping to match literally.
	store := &fakeStore{}
	engine, err := NewEngine([]Rule{{Decision: DecisionDeny, Name: "echo", Argument: `*"hi"*`}}, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{
		Name:  "echo",
		Input: json.RawMessage(`{"msg":"hi"}`),
	})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionDeny {
		t.Fatalf("Evaluate() = %v, want %v (default projector must expose the raw input verbatim)", got, DecisionDeny)
	}
}

func TestEngine_Evaluate_WithProjectorOverridesDefault(t *testing.T) {
	store := &fakeStore{}
	projector := func(json.RawMessage) string { return "always-same" }
	engine, err := NewEngine(
		[]Rule{{Decision: DecisionDeny, Name: "echo", Argument: "always-same"}},
		store,
		WithProjector(projector),
	)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{
		Name:  "echo",
		Input: json.RawMessage(`{"msg":"hi"}`),
	})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionDeny {
		t.Fatalf("Evaluate() = %v, want %v (WithProjector must override ProjectRaw)", got, DecisionDeny)
	}
}

func TestEngine_Remember_ValidatesThenAppends(t *testing.T) {
	store := &fakeStore{}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	if err := engine.Remember(context.Background(), Rule{Decision: DecisionAllow, Name: "bash"}); err != nil {
		t.Fatalf("Remember() error = %v", err)
	}
	if len(store.appended) != 1 || store.appended[0].Name != "bash" {
		t.Fatalf("Remember() appended = %+v, want exactly one Rule named bash", store.appended)
	}
}

func TestEngine_Remember_RejectsInvalidPatternWithoutAppending(t *testing.T) {
	store := &fakeStore{}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	if err := engine.Remember(context.Background(), Rule{Decision: DecisionAllow, Name: "["}); err == nil {
		t.Fatalf("Remember() with an invalid pattern error = nil, want non-nil")
	}
	if len(store.appended) != 0 {
		t.Fatalf("Remember() appended an invalid rule: %+v, want none", store.appended)
	}
}

func TestEngine_Remember_PropagatesStoreAppendError(t *testing.T) {
	store := &fakeStore{appendErr: errors.New("disk full")}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	if err := engine.Remember(context.Background(), Rule{Decision: DecisionAllow, Name: "bash"}); err == nil {
		t.Fatalf("Remember() error = nil, want non-nil when Store.Append fails")
	}
}

func TestNewEngine_ErrorsOnInvalidGlobalDenyGlob(t *testing.T) {
	_, err := NewEngine(nil, &fakeStore{}, WithGlobalDenies([]Rule{{Decision: DecisionDeny, Name: "["}}))
	if err == nil {
		t.Fatalf("NewEngine() with an invalid global-deny glob pattern error = nil, want non-nil")
	}
}

// The denied command intentionally has no "/" in it: doublestar's glob
// matching is path-segment-based (the same behavior rule_test.go's "single
// star does not cross a path separator" pins for filesystem arguments), so a
// literal "/" splits the match into segments a bare "*" cannot bridge. That
// is a real, pre-existing limitation of the unchanged doublestar-based
// evaluate() this Engine builds on — matchers meant to also block a
// slash-containing invocation like "rm -rf /" need a "**" written as its own
// path segment (e.g. "rm -rf /**"), not a bare "*". These tests instead pin
// the hard pre-check's precedence: whatever the matcher matches, a global
// deny hit must be unreachable by any allow, static or persisted.
func TestEngine_Evaluate_GlobalDenyShortCircuitsProjectAllow(t *testing.T) {
	engine, err := NewEngine(
		[]Rule{{Decision: DecisionAllow, Name: "bash"}},
		NewMemoryStore(),
		WithProjector(ProjectField),
		WithGlobalDenies([]Rule{{Decision: DecisionDeny, Name: "bash", Argument: "rm -rf *"}}),
	)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{
		Name:  "bash",
		Input: json.RawMessage(`{"command":"rm -rf tmp"}`),
	})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionDeny {
		t.Fatalf("Evaluate() = %v, want %v (a static allow must never punch through a global deny)", got, DecisionDeny)
	}
}

func TestEngine_Evaluate_GlobalDenyIsUnreachableByAPersistedAllow(t *testing.T) {
	store := NewMemoryStore()
	if err := store.Append(context.Background(), Rule{Decision: DecisionAllow, Name: "bash", Argument: "rm -rf *"}); err != nil {
		t.Fatalf("store.Append() error = %v", err)
	}
	engine, err := NewEngine(
		nil,
		store,
		WithProjector(ProjectField),
		WithGlobalDenies([]Rule{{Decision: DecisionDeny, Name: "bash", Argument: "rm -rf *"}}),
	)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{
		Name:  "bash",
		Input: json.RawMessage(`{"command":"rm -rf tmp"}`),
	})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionDeny {
		t.Fatalf("Evaluate() = %v, want %v (a persisted allow-always must never loosen a global deny)", got, DecisionDeny)
	}
}

func TestEngine_Evaluate_GlobalDenyDoesNotBlockADifferentArgument(t *testing.T) {
	engine, err := NewEngine(
		[]Rule{{Decision: DecisionAllow, Name: "bash"}},
		NewMemoryStore(),
		WithProjector(ProjectField),
		WithGlobalDenies([]Rule{{Decision: DecisionDeny, Name: "bash", Argument: "rm -rf *"}}),
	)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{
		Name:  "bash",
		Input: json.RawMessage(`{"command":"git status"}`),
	})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionAllow {
		t.Fatalf("Evaluate() = %v, want %v (the global deny's argument glob must not match an unrelated command)", got, DecisionAllow)
	}
}

func TestEngine_RememberCall_ScopesGrantToTheTriggeringArgument(t *testing.T) {
	store := NewMemoryStore()
	engine, err := NewEngine(nil, store, WithProjector(ProjectField))
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	granted := message.ToolUsePart{Name: "bash", Input: json.RawMessage(`{"command":"git status"}`)}
	if err := engine.RememberCall(context.Background(), DecisionAllow, granted); err != nil {
		t.Fatalf("RememberCall() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), granted)
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionAllow {
		t.Fatalf("Evaluate() for the granted call = %v, want %v", got, DecisionAllow)
	}

	differentArg := message.ToolUsePart{Name: "bash", Input: json.RawMessage(`{"command":"git push"}`)}
	got2, err := engine.Evaluate(context.Background(), differentArg)
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got2 != DecisionAsk {
		t.Fatalf("Evaluate() for a different argument = %v, want %v (a grant must not widen to a different argument)", got2, DecisionAsk)
	}
}

func TestEngine_RememberCall_EscapesGlobMetacharactersInTheArgument(t *testing.T) {
	store := NewMemoryStore()
	engine, err := NewEngine(nil, store, WithProjector(ProjectField))
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	granted := message.ToolUsePart{Name: "bash", Input: json.RawMessage(`{"command":"ls *.go"}`)}
	if err := engine.RememberCall(context.Background(), DecisionAllow, granted); err != nil {
		t.Fatalf("RememberCall() error = %v", err)
	}

	other := message.ToolUsePart{Name: "bash", Input: json.RawMessage(`{"command":"ls main.go"}`)}
	got, err := engine.Evaluate(context.Background(), other)
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionAsk {
		t.Fatalf("Evaluate() = %v, want %v (the persisted argument must be literal, not a live glob)", got, DecisionAsk)
	}
}

// The next three tests pin Evaluate's chat-mode hard pre-check (hard-check #2)
// directly at the Engine level: WithMode/WithWriteClassifier were built as
// scaffolding in an earlier batch, unused by any real composition root until
// this one wires them into internal/agent. These are approval tests for that
// pre-existing Evaluate behavior, run ahead of (and pinning the contract for)
// the composition-root wiring exercised in internal/agent's own tests.
func TestEngine_Evaluate_ChatModeBlocksAllowListedWrite(t *testing.T) {
	engine, err := NewEngine(
		[]Rule{{Decision: DecisionAllow, Name: "edit"}},
		NewMemoryStore(),
		WithMode(func() Mode { return ModeChat }),
		WithWriteClassifier(func(name string) bool { return name == "edit" }),
	)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{Name: "edit"})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionDeny {
		t.Fatalf("Evaluate() = %v, want %v (chat mode must block a write even when statically allow-listed)", got, DecisionDeny)
	}
}

func TestEngine_Evaluate_EditModeHonorsAllowRule(t *testing.T) {
	engine, err := NewEngine(
		[]Rule{{Decision: DecisionAllow, Name: "edit"}},
		NewMemoryStore(),
		WithMode(func() Mode { return ModeEdit }),
		WithWriteClassifier(func(name string) bool { return name == "edit" }),
	)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{Name: "edit"})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionAllow {
		t.Fatalf("Evaluate() = %v, want %v (edit mode must honor the static allow rule; the same rule that chat mode blocks above)", got, DecisionAllow)
	}
}

func TestEngine_Evaluate_ChatModeDoesNotBlockANonWriteTool(t *testing.T) {
	engine, err := NewEngine(
		nil,
		NewMemoryStore(),
		WithMode(func() Mode { return ModeChat }),
		WithWriteClassifier(func(name string) bool { return name == "edit" }),
	)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	got, err := engine.Evaluate(context.Background(), message.ToolUsePart{Name: "read"})
	if err != nil {
		t.Fatalf("Evaluate() error = %v", err)
	}
	if got != DecisionAsk {
		t.Fatalf("Evaluate() = %v, want %v (chat mode only blocks tools the classifier flags as a write)", got, DecisionAsk)
	}
}

func TestEngine_RememberCall_ArgumentlessCallFallsBackToNameScoped(t *testing.T) {
	store := NewMemoryStore()
	engine, err := NewEngine(nil, store, WithProjector(ProjectField))
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}

	call := message.ToolUsePart{Name: "task"}
	if err := engine.RememberCall(context.Background(), DecisionAllow, call); err != nil {
		t.Fatalf("RememberCall() error = %v", err)
	}

	rules, _ := store.Rules(context.Background())
	if len(rules) != 1 || rules[0].Argument != "" {
		t.Fatalf("store rules = %+v, want exactly one rule with an empty Argument", rules)
	}
}
