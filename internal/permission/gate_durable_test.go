package permission_test

import (
	"context"
	"errors"
	"path/filepath"
	"testing"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/permission"
	"github.com/0xErwin1/agens/internal/permission/permissiondb"
	"github.com/0xErwin1/agens/internal/provider"
)

type durableRunner struct {
	calls int
}

func (r *durableRunner) Specs() []provider.ToolSpec { return nil }

func (r *durableRunner) Run(context.Context, message.ToolUsePart) (message.ToolResultPart, error) {
	r.calls++
	return message.ToolResultPart{}, nil
}

type forbiddenDurablePrompter struct {
	calls int
}

func (p *forbiddenDurablePrompter) Prompt(context.Context, message.ToolUsePart) (permission.Answer, error) {
	p.calls++
	return permission.AnswerCancel, errors.New("Prompt must not be called")
}

func TestGateBypassActiveLeavesDurableStoreUnchanged(t *testing.T) {
	store, err := permissiondb.Open(filepath.Join(t.TempDir(), "permissions.db"), t.TempDir())
	if err != nil {
		t.Fatalf("permissiondb.Open() error = %v", err)
	}

	inner := &durableRunner{}
	prompter := &forbiddenDurablePrompter{}
	engine, err := permission.NewEngine(nil, store)
	if err != nil {
		t.Fatalf("permission.NewEngine() error = %v", err)
	}

	gate := permission.NewGate(inner, engine, prompter, permission.WithBypass(func() bool { return true }))
	if _, err := gate.Run(context.Background(), message.ToolUsePart{ID: "c1", Name: "bash"}); err != nil {
		t.Fatalf("Run() error = %v, want nil", err)
	}

	rules, err := store.Rules(context.Background())
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	if len(rules) != 0 {
		t.Fatalf("Rules() = %+v, want no durable permission rows", rules)
	}
	if inner.calls != 1 {
		t.Fatalf("inner.calls = %d, want 1", inner.calls)
	}
	if prompter.calls != 0 {
		t.Fatalf("prompter.calls = %d, want 0", prompter.calls)
	}
}
