package permission

import (
	"context"
	"errors"
	"reflect"
	"strings"
	"testing"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/provider"
)

// fakeRunner is a Runner double: it records how many times Run was called
// and with which call, and returns canned Specs/result/error.
type fakeRunner struct {
	calls    int
	lastCall message.ToolUsePart
	result   message.ToolResultPart
	err      error
	specs    []provider.ToolSpec
}

func (f *fakeRunner) Specs() []provider.ToolSpec { return f.specs }

func (f *fakeRunner) Run(ctx context.Context, call message.ToolUsePart) (message.ToolResultPart, error) {
	f.calls++
	f.lastCall = call
	return f.result, f.err
}

var _ Runner = (*fakeRunner)(nil)

// fakePrompter is a Prompter double: it records how many times Prompt was
// called, returns a canned Answer/error, and optionally runs onCall before
// returning (used to simulate a ctx cancellation racing the Prompter).
type fakePrompter struct {
	calls  int
	answer Answer
	err    error
	onCall func()
}

func (f *fakePrompter) Prompt(ctx context.Context, call message.ToolUsePart) (Answer, error) {
	f.calls++
	if f.onCall != nil {
		f.onCall()
	}
	return f.answer, f.err
}

var _ Prompter = (*fakePrompter)(nil)

func TestGate_CtxAlreadyCancelled(t *testing.T) {
	inner := &fakeRunner{}
	prompter := &fakePrompter{}
	engine, err := NewEngine(nil, NewMemoryStore())
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	_, err = gate.Run(ctx, message.ToolUsePart{ID: "c1", Name: "bash"})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if inner.calls != 0 {
		t.Fatalf("inner.calls = %d, want 0 (tool must not execute)", inner.calls)
	}
	if prompter.calls != 0 {
		t.Fatalf("prompter.calls = %d, want 0 (Prompter must not be consulted)", prompter.calls)
	}
}

func TestGate_Allow_Delegates(t *testing.T) {
	wantResult := message.ToolResultPart{ToolUseID: "c1", Content: message.Parts{message.TextPart{Text: "ok"}}}
	wantErr := errors.New("inner error")
	inner := &fakeRunner{result: wantResult, err: wantErr}
	prompter := &fakePrompter{}
	engine, err := NewEngine([]Rule{{Decision: DecisionAllow, Name: "*"}}, NewMemoryStore())
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	got, gotErr := gate.Run(context.Background(), call)

	if inner.calls != 1 {
		t.Fatalf("inner.calls = %d, want 1", inner.calls)
	}
	if !reflect.DeepEqual(inner.lastCall, call) {
		t.Fatalf("inner received call = %+v, want %+v", inner.lastCall, call)
	}
	if !reflect.DeepEqual(got, wantResult) {
		t.Fatalf("Run() result = %+v, want %+v (passthrough)", got, wantResult)
	}
	if !errors.Is(gotErr, wantErr) {
		t.Fatalf("Run() error = %v, want %v (passthrough)", gotErr, wantErr)
	}
	if prompter.calls != 0 {
		t.Fatalf("prompter.calls = %d, want 0 (Allow must not consult the Prompter)", prompter.calls)
	}
}

func TestGate_Deny_ShortCircuits(t *testing.T) {
	inner := &fakeRunner{}
	prompter := &fakePrompter{}
	engine, err := NewEngine([]Rule{{Decision: DecisionDeny, Name: "*"}}, NewMemoryStore())
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	got, err := gate.Run(context.Background(), call)

	if err != nil {
		t.Fatalf("Run() error = %v, want nil (a denial is a tool-level result, not a Go error)", err)
	}
	if inner.calls != 0 {
		t.Fatalf("inner.calls = %d, want 0", inner.calls)
	}
	if !got.IsError {
		t.Fatalf("IsError = false, want true")
	}
	if got.ToolUseID != "c1" {
		t.Fatalf("ToolUseID = %q, want %q", got.ToolUseID, "c1")
	}
	if len(got.Content) != 1 {
		t.Fatalf("Content = %+v, want exactly 1 part", got.Content)
	}
	text, ok := got.Content[0].(message.TextPart)
	if !ok || !strings.Contains(text.Text, "bash") {
		t.Fatalf("Content[0] = %+v, want a TextPart mentioning %q", got.Content[0], "bash")
	}
}

func TestGate_Ask_AllowOnce_DelegatesWithoutPersisting(t *testing.T) {
	store := NewMemoryStore()
	wantResult := message.ToolResultPart{ToolUseID: "c1", Content: message.Parts{message.TextPart{Text: "ok"}}}
	inner := &fakeRunner{result: wantResult}
	prompter := &fakePrompter{answer: AnswerAllowOnce}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	got, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if inner.calls != 1 {
		t.Fatalf("inner.calls = %d, want 1", inner.calls)
	}
	if !reflect.DeepEqual(got, wantResult) {
		t.Fatalf("Run() result = %+v, want %+v", got, wantResult)
	}
	if prompter.calls != 1 {
		t.Fatalf("prompter.calls = %d, want 1", prompter.calls)
	}

	rules, _ := store.Rules(context.Background())
	if len(rules) != 0 {
		t.Fatalf("store rules = %+v, want none appended for allow-once", rules)
	}

	if _, err := gate.Run(context.Background(), call); err != nil {
		t.Fatalf("second Run() error = %v, want nil", err)
	}
	if prompter.calls != 2 {
		t.Fatalf("prompter.calls after second identical call = %d, want 2 (allow-once must not skip the prompt)", prompter.calls)
	}
}

func TestGate_Ask_AllowAlways_AppendsRuleAndSecondCallSkipsPrompt(t *testing.T) {
	store := NewMemoryStore()
	wantResult := message.ToolResultPart{ToolUseID: "c1", Content: message.Parts{message.TextPart{Text: "ok"}}}
	inner := &fakeRunner{result: wantResult}
	prompter := &fakePrompter{answer: AnswerAllowAlways}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	got, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if !reflect.DeepEqual(got, wantResult) {
		t.Fatalf("Run() result = %+v, want %+v", got, wantResult)
	}
	if inner.calls != 1 {
		t.Fatalf("inner.calls = %d, want 1", inner.calls)
	}

	rules, _ := store.Rules(context.Background())
	if len(rules) != 1 || rules[0].Decision != DecisionAllow || rules[0].Name != "bash" {
		t.Fatalf("store rules = %+v, want exactly one Allow rule named bash", rules)
	}

	if _, err := gate.Run(context.Background(), call); err != nil {
		t.Fatalf("second Run() error = %v, want nil", err)
	}
	if inner.calls != 2 {
		t.Fatalf("inner.calls after second call = %d, want 2 (allow-always must still execute)", inner.calls)
	}
	if prompter.calls != 1 {
		t.Fatalf("prompter.calls after second call = %d, want 1 (allow-always must skip the prompt on a later matching call)", prompter.calls)
	}
}

func TestGate_Ask_DenyOnce_DoesNotPersist(t *testing.T) {
	store := NewMemoryStore()
	inner := &fakeRunner{}
	prompter := &fakePrompter{answer: AnswerDenyOnce}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	got, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if inner.calls != 0 {
		t.Fatalf("inner.calls = %d, want 0", inner.calls)
	}
	if !got.IsError {
		t.Fatalf("IsError = false, want true")
	}

	rules, _ := store.Rules(context.Background())
	if len(rules) != 0 {
		t.Fatalf("store rules = %+v, want none appended for deny-once", rules)
	}
}

func TestGate_Ask_DenyAlways_AppendsRuleAndSecondCallDeniedWithoutPrompt(t *testing.T) {
	store := NewMemoryStore()
	inner := &fakeRunner{}
	prompter := &fakePrompter{answer: AnswerDenyAlways}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	got, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}
	if !got.IsError {
		t.Fatalf("IsError = false, want true")
	}

	rules, _ := store.Rules(context.Background())
	if len(rules) != 1 || rules[0].Decision != DecisionDeny || rules[0].Name != "bash" {
		t.Fatalf("store rules = %+v, want exactly one Deny rule named bash", rules)
	}

	got2, err := gate.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("second Run() error = %v, want nil", err)
	}
	if !got2.IsError {
		t.Fatalf("second IsError = false, want true")
	}
	if inner.calls != 0 {
		t.Fatalf("inner.calls = %d, want 0", inner.calls)
	}
	if prompter.calls != 1 {
		t.Fatalf("prompter.calls after second call = %d, want 1 (deny-always must skip the prompt on a later matching call)", prompter.calls)
	}
}

func TestGate_Ask_Cancel(t *testing.T) {
	store := NewMemoryStore()
	inner := &fakeRunner{}
	prompter := &fakePrompter{answer: AnswerCancel}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	_, err = gate.Run(context.Background(), call)
	if !errors.Is(err, ErrCanceled) {
		t.Fatalf("Run() error = %v, want errors.Is(err, ErrCanceled)", err)
	}
	if inner.calls != 0 {
		t.Fatalf("inner.calls = %d, want 0", inner.calls)
	}

	rules, _ := store.Rules(context.Background())
	if len(rules) != 0 {
		t.Fatalf("store rules = %+v, want none appended on cancel", rules)
	}
}

func TestGate_PrompterErrorTakesCtxPriority(t *testing.T) {
	store := NewMemoryStore()
	inner := &fakeRunner{}
	ctx, cancel := context.WithCancel(context.Background())
	prompter := &fakePrompter{err: errors.New("boom"), onCall: cancel}
	engine, err := NewEngine(nil, store)
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, prompter)

	call := message.ToolUsePart{ID: "c1", Name: "bash"}
	_, err = gate.Run(ctx, call)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want errors.Is(err, context.Canceled) (ctx-priority over the Prompter's own error)", err)
	}
	if inner.calls != 0 {
		t.Fatalf("inner.calls = %d, want 0", inner.calls)
	}
}

func TestGate_Specs_PassThrough(t *testing.T) {
	wantSpecs := []provider.ToolSpec{{Name: "a"}, {Name: "b"}}
	inner := &fakeRunner{specs: wantSpecs}
	engine, err := NewEngine(nil, NewMemoryStore())
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	gate := NewGate(inner, engine, &fakePrompter{})

	if !reflect.DeepEqual(gate.Specs(), wantSpecs) {
		t.Fatalf("Specs() = %+v, want %+v", gate.Specs(), wantSpecs)
	}
}

func TestNewGate_PanicsOnNil(t *testing.T) {
	engine, err := NewEngine(nil, NewMemoryStore())
	if err != nil {
		t.Fatalf("NewEngine() error = %v", err)
	}
	inner := &fakeRunner{}
	prompter := &fakePrompter{}

	tests := []struct {
		name     string
		inner    Runner
		engine   *Engine
		prompter Prompter
	}{
		{name: "nil inner", inner: nil, engine: engine, prompter: prompter},
		{name: "nil engine", inner: inner, engine: nil, prompter: prompter},
		{name: "nil prompter", inner: inner, engine: engine, prompter: nil},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			defer func() {
				if recover() == nil {
					t.Fatalf("NewGate(%s) did not panic, want a panic", tt.name)
				}
			}()
			NewGate(tt.inner, tt.engine, tt.prompter)
		})
	}
}
