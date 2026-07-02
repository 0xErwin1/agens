package permission

import (
	"context"
	"errors"

	"github.com/iperez/agens/internal/message"
)

// Answer is how a Prompter resolves an Ask decision for one call.
type Answer int

const (
	// AnswerDenyOnce is the zero value: the fail-safe default when no
	// Prompter implementation has set an explicit answer.
	AnswerDenyOnce Answer = iota
	AnswerDenyAlways
	AnswerAllowOnce
	AnswerAllowAlways
	AnswerCancel
)

// ErrCanceled is returned by a Prompter (or synthesized by a Gate) when the
// user cancels a permission prompt.
var ErrCanceled = errors.New("permission: prompt canceled")

// Prompter resolves an Ask decision by asking some outside party (a human,
// a scripted test double, or a future non-interactive policy). Prompt must
// honor ctx promptly and return ctx.Err() as a real error when ctx is
// canceled while waiting for an answer.
type Prompter interface {
	Prompt(ctx context.Context, call message.ToolUsePart) (Answer, error)
}

// DenyPrompter answers AnswerDenyOnce to every prompt without blocking. It
// is the safe default for non-interactive surfaces that have not yet wired
// a real Prompter: every Ask decision is denied until one is.
type DenyPrompter struct{}

func (DenyPrompter) Prompt(ctx context.Context, call message.ToolUsePart) (Answer, error) {
	return AnswerDenyOnce, nil
}

var _ Prompter = DenyPrompter{}
