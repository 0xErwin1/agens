package cli

import (
	"bytes"
	"context"
	"errors"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/permission"
)

func TestTTYPrompter_KeyMap(t *testing.T) {
	tests := []struct {
		name  string
		input string
		want  permission.Answer
	}{
		{name: "y allows once", input: "y\n", want: permission.AnswerAllowOnce},
		{name: "a allows always", input: "a\n", want: permission.AnswerAllowAlways},
		{name: "n denies once", input: "n\n", want: permission.AnswerDenyOnce},
		{name: "d denies always", input: "d\n", want: permission.AnswerDenyAlways},
		{name: "q cancels", input: "q\n", want: permission.AnswerCancel},
		{name: "uppercase Y allows once", input: "Y\n", want: permission.AnswerAllowOnce},
		{name: "whitespace-padded a allows always", input: "  a  \n", want: permission.AnswerAllowAlways},
		{name: "garbage is deny-once", input: "banana\n", want: permission.AnswerDenyOnce},
		{name: "empty line is deny-once", input: "\n", want: permission.AnswerDenyOnce},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			out := new(bytes.Buffer)
			p := newTTYPrompter(strings.NewReader(tt.input), out)

			answer, err := p.Prompt(context.Background(), message.ToolUsePart{Name: "write"})
			if err != nil {
				t.Fatalf("Prompt() error = %v, want nil", err)
			}
			if answer != tt.want {
				t.Fatalf("Prompt() answer = %v, want %v", answer, tt.want)
			}
		})
	}
}

func TestTTYPrompter_EOFIsDenyOnceWithoutError(t *testing.T) {
	out := new(bytes.Buffer)
	p := newTTYPrompter(strings.NewReader(""), out)

	answer, err := p.Prompt(context.Background(), message.ToolUsePart{Name: "write"})
	if err != nil {
		t.Fatalf("Prompt() error = %v, want nil (EOF mirrors DenyPrompter)", err)
	}
	if answer != permission.AnswerDenyOnce {
		t.Fatalf("Prompt() answer = %v, want %v", answer, permission.AnswerDenyOnce)
	}
}

type errReader struct{ err error }

func (r errReader) Read([]byte) (int, error) { return 0, r.err }

func TestTTYPrompter_NonEOFReadErrorIsRealError(t *testing.T) {
	wantErr := errors.New("boom: tty read failed")
	out := new(bytes.Buffer)
	p := newTTYPrompter(errReader{err: wantErr}, out)

	_, err := p.Prompt(context.Background(), message.ToolUsePart{Name: "write"})
	if !errors.Is(err, wantErr) {
		t.Fatalf("Prompt() error = %v, want it to wrap %v", err, wantErr)
	}
}

func TestTTYPrompter_CtxCancelMidPromptReturnsCtxErr(t *testing.T) {
	pr, pw := io.Pipe()
	defer func() { _ = pw.Close() }()
	out := new(bytes.Buffer)
	p := newTTYPrompter(pr, out)

	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	_, err := p.Prompt(ctx, message.ToolUsePart{Name: "write"})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Prompt() error = %v, want errors.Is(err, context.Canceled)", err)
	}
}

func TestTTYPrompter_OutputIncludesToolNameAndPath(t *testing.T) {
	out := new(bytes.Buffer)
	p := newTTYPrompter(strings.NewReader("n\n"), out)

	call := message.ToolUsePart{Name: "write", Input: []byte(`{"path":"internal/agent/agent.go","content":"x"}`)}
	if _, err := p.Prompt(context.Background(), call); err != nil {
		t.Fatalf("Prompt() error = %v, want nil", err)
	}

	got := out.String()
	if !strings.Contains(got, "write") {
		t.Fatalf("prompt output = %q, want it to mention the tool name %q", got, "write")
	}
	if !strings.Contains(got, "internal/agent/agent.go") {
		t.Fatalf("prompt output = %q, want it to mention the path %q", got, "internal/agent/agent.go")
	}
}

func TestTTYPrompter_OutputFallsBackToRawInputWhenNoPath(t *testing.T) {
	out := new(bytes.Buffer)
	p := newTTYPrompter(strings.NewReader("n\n"), out)

	call := message.ToolUsePart{Name: "bash", Input: []byte(`{"command":"ls"}`)}
	if _, err := p.Prompt(context.Background(), call); err != nil {
		t.Fatalf("Prompt() error = %v, want nil", err)
	}

	got := out.String()
	if !strings.Contains(got, "bash") {
		t.Fatalf("prompt output = %q, want it to mention the tool name %q", got, "bash")
	}
	if !strings.Contains(got, `"command":"ls"`) {
		t.Fatalf("prompt output = %q, want it to fall back to the raw input when no path field is present", got)
	}
}

func TestSelectPrompter_AllowAllReturnsAllowPrompter(t *testing.T) {
	p := selectPrompter(true)
	if _, ok := p.(permission.AllowPrompter); !ok {
		t.Fatalf("selectPrompter(true) = %T, want permission.AllowPrompter", p)
	}
}

var (
	_ permission.Prompter = (*ttyPrompter)(nil)
)
