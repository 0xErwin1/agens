package cli

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/permission"
)

// ttyPrompter asks a human for a permission decision by writing a prompt to
// out and reading a single line of input from in. Production code wraps
// /dev/tty for both fields, so prompting works even when the chat prompt
// itself is piped through stdin; tests inject scripted readers/writers
// instead of a real terminal.
type ttyPrompter struct {
	in  io.Reader
	out io.Writer
}

// newTTYPrompter returns a ttyPrompter that writes its prompt to out and
// reads the answer from in.
func newTTYPrompter(in io.Reader, out io.Writer) *ttyPrompter {
	return &ttyPrompter{in: in, out: out}
}

var _ permission.Prompter = (*ttyPrompter)(nil)

// Prompt writes call's tool name and a best-effort argument to p.out, then
// reads one line of input from p.in and maps it to a permission.Answer.
//
// The line read runs in a goroutine so Prompt can select on ctx.Done():
// ctx cancellation returns ctx.Err() immediately, promptly honoring ctx as
// permission.Prompter requires, even though the goroutine itself may keep
// blocking on in until it is closed or produces a byte (accepted for
// one-shot chat, where the process exits shortly after).
func (p *ttyPrompter) Prompt(ctx context.Context, call message.ToolUsePart) (permission.Answer, error) {
	if _, err := fmt.Fprintf(p.out, "%s %s — allow? [y]es once / [a]lways / [n]o / [d]eny always / [q]uit: ", call.Name, permission.ProjectField(call.Input)); err != nil {
		return permission.AnswerDenyOnce, fmt.Errorf("cli: write permission prompt: %w", err)
	}

	type lineResult struct {
		line string
		err  error
	}
	resultCh := make(chan lineResult, 1)
	go func() {
		line, err := bufio.NewReader(p.in).ReadString('\n')
		resultCh <- lineResult{line: line, err: err}
	}()

	select {
	case <-ctx.Done():
		return permission.AnswerDenyOnce, ctx.Err()
	case res := <-resultCh:
		if res.err != nil {
			if errors.Is(res.err, io.EOF) {
				return permission.AnswerDenyOnce, nil
			}
			return permission.AnswerDenyOnce, fmt.Errorf("cli: read permission answer: %w", res.err)
		}
		return answerForKey(res.line), nil
	}
}

// answerForKey maps a single line of scripted or human input to a
// permission.Answer. Matching is case-insensitive and trims surrounding
// whitespace; anything unrecognized, including an empty line, resolves to
// AnswerDenyOnce, the same fail-safe zero value permission.Answer already
// defaults to.
func answerForKey(line string) permission.Answer {
	switch strings.ToLower(strings.TrimSpace(line)) {
	case "y":
		return permission.AnswerAllowOnce
	case "a":
		return permission.AnswerAllowAlways
	case "n":
		return permission.AnswerDenyOnce
	case "d":
		return permission.AnswerDenyAlways
	case "q":
		return permission.AnswerCancel
	default:
		return permission.AnswerDenyOnce
	}
}

// selectPrompter resolves the permission.Prompter used for the chat
// command's Ask decisions. allowAll (the --dangerously-allow-all flag)
// takes priority over terminal detection and always resolves to
// permission.AllowPrompter. Otherwise /dev/tty is opened for both prompt
// output and answer input: opening it successfully is itself the signal
// that a controlling terminal is present, and its failure (no controlling
// terminal, e.g. CI or a fully piped invocation) falls back to
// permission.DenyPrompter, which denies every Ask decision.
func selectPrompter(allowAll bool) permission.Prompter {
	if allowAll {
		return permission.AllowPrompter{}
	}

	tty, err := os.OpenFile("/dev/tty", os.O_RDWR, 0)
	if err != nil {
		return permission.DenyPrompter{}
	}
	return newTTYPrompter(tty, tty)
}
