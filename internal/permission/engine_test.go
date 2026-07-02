package permission

import (
	"context"
	"encoding/json"
	"errors"
	"testing"

	"github.com/iperez/agens/internal/message"
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
