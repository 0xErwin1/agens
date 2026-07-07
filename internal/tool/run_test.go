package tool

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/0xErwin1/agens/internal/agentloop"
	"github.com/0xErwin1/agens/internal/message"
)

// var _ agentloop.ToolRunner = (*Registry)(nil) is the only place internal/tool
// imports internal/agentloop, verifying at compile time that *Registry
// structurally satisfies agentloop.ToolRunner without any production code
// depending on agentloop.
var _ agentloop.ToolRunner = (*Registry)(nil)

func TestRun_KnownToolSuccess(t *testing.T) {
	var gotInput json.RawMessage
	r := NewRegistry()
	r.Register(&fakeTool{
		name: "echo",
		execute: func(ctx context.Context, input json.RawMessage) (Result, error) {
			gotInput = input
			return Result{Text: "hi", IsError: false}, nil
		},
	})

	call := message.ToolUsePart{ID: "c1", Name: "echo", Input: json.RawMessage(`{"msg":"hi"}`)}

	result, err := r.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}

	if string(gotInput) != `{"msg":"hi"}` {
		t.Fatalf("Execute received Input = %s, want %s", gotInput, call.Input)
	}
	if result.ToolUseID != "c1" {
		t.Fatalf("ToolUseID = %q, want %q", result.ToolUseID, "c1")
	}
	if result.IsError {
		t.Fatalf("IsError = true, want false")
	}
	if len(result.Content) != 1 {
		t.Fatalf("Content = %+v, want exactly 1 part", result.Content)
	}
	text, ok := result.Content[0].(message.TextPart)
	if !ok || text.Text != "hi" {
		t.Fatalf("Content[0] = %+v, want TextPart{Text: %q}", result.Content[0], "hi")
	}
}

func TestRun_UnknownTool(t *testing.T) {
	r := NewRegistry()
	call := message.ToolUsePart{ID: "c2", Name: "ghost"}

	result, err := r.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil (unknown tool is not a Go error)", err)
	}

	if result.ToolUseID != "c2" {
		t.Fatalf("ToolUseID = %q, want %q", result.ToolUseID, "c2")
	}
	if !result.IsError {
		t.Fatalf("IsError = false, want true")
	}
	if len(result.Content) != 1 {
		t.Fatalf("Content = %+v, want exactly 1 part", result.Content)
	}
	text, ok := result.Content[0].(message.TextPart)
	if !ok || !strings.Contains(text.Text, "ghost") {
		t.Fatalf("Content[0] = %+v, want a TextPart mentioning %q", result.Content[0], "ghost")
	}
}

func TestRun_ExecuteErrorNonCtx(t *testing.T) {
	r := NewRegistry()
	r.Register(&fakeTool{
		name: "fail",
		execute: func(ctx context.Context, input json.RawMessage) (Result, error) {
			return Result{}, errors.New("boom")
		},
	})

	call := message.ToolUsePart{ID: "c3", Name: "fail"}

	result, err := r.Run(context.Background(), call)
	if err != nil {
		t.Fatalf("Run() error = %v, want nil (non-ctx Execute error must not abort)", err)
	}

	if result.ToolUseID != "c3" {
		t.Fatalf("ToolUseID = %q, want %q", result.ToolUseID, "c3")
	}
	if !result.IsError {
		t.Fatalf("IsError = false, want true")
	}
	text, ok := result.Content[0].(message.TextPart)
	if !ok || text.Text != "boom" {
		t.Fatalf("Content[0] = %+v, want TextPart{Text: %q}", result.Content[0], "boom")
	}
}

func TestRun_CtxCancelledBeforeDispatch(t *testing.T) {
	executed := false
	r := NewRegistry()
	r.Register(&fakeTool{
		name: "slow",
		execute: func(ctx context.Context, input json.RawMessage) (Result, error) {
			executed = true
			return Result{Text: "should not run"}, nil
		},
	})

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	call := message.ToolUsePart{ID: "c4", Name: "slow"}

	result, err := r.Run(ctx, call)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if result.ToolUseID != "" || result.IsError {
		t.Fatalf("result = %+v, want the zero ToolResultPart on cancellation", result)
	}
	if executed {
		t.Fatalf("Execute was called, want ctx.Err() checked before dispatch")
	}
}

func TestRun_CtxCancelledDuringExecute(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())

	r := NewRegistry()
	r.Register(&fakeTool{
		name: "slow",
		execute: func(ec context.Context, input json.RawMessage) (Result, error) {
			// Cancel mid-execution so Run enters with a live ctx (passing the
			// pre-dispatch guard) and only sees the cancellation after Execute
			// returns, exercising the post-Execute cancellation branch.
			cancel()
			return Result{}, ec.Err()
		},
	})

	call := message.ToolUsePart{ID: "c4", Name: "slow"}

	result, err := r.Run(ctx, call)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Run() error = %v, want errors.Is(err, context.Canceled)", err)
	}
	if result.ToolUseID != "" || result.IsError {
		t.Fatalf("result = %+v, want the zero ToolResultPart on cancellation", result)
	}
}

func TestRun_PassesCtxToExecute(t *testing.T) {
	type ctxKey struct{}
	want := context.WithValue(context.Background(), ctxKey{}, "marker")

	var got context.Context
	r := NewRegistry()
	r.Register(&fakeTool{
		name: "capture",
		execute: func(ctx context.Context, input json.RawMessage) (Result, error) {
			got = ctx
			return Result{}, nil
		},
	})

	if _, err := r.Run(want, message.ToolUsePart{ID: "c5", Name: "capture"}); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}

	if got != want {
		t.Fatalf("Execute received a different ctx than Run was called with")
	}
}
