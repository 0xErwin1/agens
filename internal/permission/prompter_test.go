package permission

import (
	"context"
	"testing"

	"github.com/0xErwin1/agens/internal/message"
)

func TestAnswer_ZeroValueIsDenyOnce(t *testing.T) {
	var a Answer
	if a != AnswerDenyOnce {
		t.Fatalf("Answer zero value = %v, want AnswerDenyOnce (fail-safe default)", a)
	}
}

func TestDenyPrompter_AlwaysDeniesOnce(t *testing.T) {
	p := DenyPrompter{}

	answer, err := p.Prompt(context.Background(), message.ToolUsePart{Name: "bash"})
	if err != nil {
		t.Fatalf("DenyPrompter.Prompt() error = %v, want nil", err)
	}
	if answer != AnswerDenyOnce {
		t.Fatalf("DenyPrompter.Prompt() answer = %v, want %v", answer, AnswerDenyOnce)
	}
}

var _ Prompter = DenyPrompter{}

func TestAllowPrompter_AlwaysAllowsOnce(t *testing.T) {
	tests := []struct {
		name string
		ctx  context.Context
		call message.ToolUsePart
	}{
		{
			name: "background ctx, write call",
			ctx:  context.Background(),
			call: message.ToolUsePart{Name: "write", Input: []byte(`{"path":"a.txt","content":"x"}`)},
		},
		{
			name: "already-canceled ctx does not change the answer",
			ctx:  canceledContext(),
			call: message.ToolUsePart{Name: "edit", Input: []byte(`{"path":"a.txt"}`)},
		},
		{
			name: "empty call still allows once",
			ctx:  context.Background(),
			call: message.ToolUsePart{},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			p := AllowPrompter{}

			answer, err := p.Prompt(tt.ctx, tt.call)
			if err != nil {
				t.Fatalf("AllowPrompter.Prompt() error = %v, want nil", err)
			}
			if answer != AnswerAllowOnce {
				t.Fatalf("AllowPrompter.Prompt() answer = %v, want %v", answer, AnswerAllowOnce)
			}
		})
	}
}

// TestAllowPrompter_NeverPersistsARule documents why AllowPrompter never
// causes Gate.runAsk to persist a rule: it only ever returns
// AnswerAllowOnce, and Gate only calls Engine.Remember for
// AnswerAllowAlways/AnswerDenyAlways.
func TestAllowPrompter_NeverPersistsARule(t *testing.T) {
	p := AllowPrompter{}

	for i := 0; i < 5; i++ {
		answer, err := p.Prompt(context.Background(), message.ToolUsePart{Name: "write"})
		if err != nil {
			t.Fatalf("AllowPrompter.Prompt() error = %v, want nil", err)
		}
		if answer == AnswerAllowAlways || answer == AnswerDenyAlways {
			t.Fatalf("AllowPrompter.Prompt() answer = %v, must never be an \"always\" answer", answer)
		}
	}
}

func canceledContext() context.Context {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	return ctx
}

var _ Prompter = AllowPrompter{}
