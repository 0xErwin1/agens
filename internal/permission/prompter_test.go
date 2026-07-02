package permission

import (
	"context"
	"testing"

	"github.com/iperez/agens/internal/message"
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
