package tui

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/permission"
)

func toolCall(name, input string) message.ToolUsePart {
	return message.ToolUsePart{ID: "tu_1", Name: name, Input: json.RawMessage(input)}
}

func TestPrompter_ForwardsRequestAndReturnsAnswer(t *testing.T) {
	p := NewPrompter()

	type result struct {
		answer permission.Answer
		err    error
	}
	done := make(chan result, 1)
	go func() {
		ans, err := p.Prompt(context.Background(), toolCall("bash", `{"command":"ls"}`))
		done <- result{ans, err}
	}()

	req := <-p.Requests()
	if req.Call.Name != "bash" {
		t.Fatalf("forwarded call name = %q, want %q", req.Call.Name, "bash")
	}
	req.Reply <- permission.AnswerAllowOnce

	got := <-done
	if got.err != nil {
		t.Fatalf("Prompt() error = %v, want nil", got.err)
	}
	if got.answer != permission.AnswerAllowOnce {
		t.Fatalf("Prompt() answer = %v, want AnswerAllowOnce", got.answer)
	}
}

func TestPrompter_CanceledBeforeHandoffReturnsCancel(t *testing.T) {
	p := NewPrompter()

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	ans, err := p.Prompt(ctx, toolCall("bash", `{}`))
	if err != context.Canceled {
		t.Fatalf("Prompt() error = %v, want context.Canceled", err)
	}
	if ans != permission.AnswerCancel {
		t.Fatalf("Prompt() answer = %v, want AnswerCancel", ans)
	}
}

func TestPrompter_CanceledWhileWaitingReturnsCancel(t *testing.T) {
	p := NewPrompter()

	ctx, cancel := context.WithCancel(context.Background())

	type result struct {
		answer permission.Answer
		err    error
	}
	done := make(chan result, 1)
	go func() {
		ans, err := p.Prompt(ctx, toolCall("write", `{"path":"a.txt"}`))
		done <- result{ans, err}
	}()

	<-p.Requests() // accept the request but never reply
	cancel()

	got := <-done
	if got.err != context.Canceled {
		t.Fatalf("Prompt() error = %v, want context.Canceled", got.err)
	}
	if got.answer != permission.AnswerCancel {
		t.Fatalf("Prompt() answer = %v, want AnswerCancel", got.answer)
	}
}

func TestAnswerForModalKey(t *testing.T) {
	cases := []struct {
		key  tea.KeyMsg
		want permission.Answer
		ok   bool
	}{
		{tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("y")}, permission.AnswerAllowOnce, true},
		{tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("a")}, permission.AnswerAllowAlways, true},
		{tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("n")}, permission.AnswerDenyOnce, true},
		{tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("d")}, permission.AnswerDenyAlways, true},
		{tea.KeyMsg{Type: tea.KeyEsc}, permission.AnswerDenyOnce, true},
		{tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("x")}, permission.AnswerDenyOnce, false},
	}
	for _, c := range cases {
		got, ok := answerForModalKey(c.key)
		if ok != c.ok || got != c.want {
			t.Fatalf("answerForModalKey(%v) = (%v, %v), want (%v, %v)", c.key, got, ok, c.want, c.ok)
		}
	}
}

func TestPermissionDetail(t *testing.T) {
	cases := []struct {
		input string
		want  string
	}{
		{`{"command":"ls -la"}`, "ls -la"},
		{`{"path":"internal/foo.go"}`, "internal/foo.go"},
		{`{"url":"https://example.com"}`, "https://example.com"},
		{`{}`, ""},
		{`null`, ""},
	}
	for _, c := range cases {
		if got := permissionDetail(json.RawMessage(c.input)); got != c.want {
			t.Fatalf("permissionDetail(%s) = %q, want %q", c.input, got, c.want)
		}
	}
}

func modalModel(t *testing.T) (*Model, PermissionRequest) {
	t.Helper()
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Prompter: NewPrompter()})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	req := PermissionRequest{
		Call:  toolCall("bash", `{"command":"ls"}`),
		Reply: make(chan permission.Answer, 1),
	}
	sendMsg(m, PermissionRequestMsg{Request: req})
	return m, req
}

func TestModel_PermissionRequestShowsModal(t *testing.T) {
	m, _ := modalModel(t)

	view := m.View()
	if !strings.Contains(view, "Permission required") {
		t.Fatalf("View() = %q, want it to show the permission modal", view)
	}
	if !strings.Contains(view, "bash") {
		t.Fatalf("View() = %q, want it to name the requested tool", view)
	}
}

func TestModel_ModalAllowSendsAnswerAndClears(t *testing.T) {
	m, req := modalModel(t)

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("y")})

	select {
	case ans := <-req.Reply:
		if ans != permission.AnswerAllowOnce {
			t.Fatalf("reply = %v, want AnswerAllowOnce", ans)
		}
	default:
		t.Fatal("pressing 'y' did not send an answer on the reply channel")
	}

	if m.pending != nil {
		t.Fatal("modal is still pending after an answer, want it cleared")
	}
	if strings.Contains(m.View(), "Permission required") {
		t.Fatal("View() still shows the modal after answering")
	}
}

func TestModel_ModalUnboundKeyKeepsModal(t *testing.T) {
	m, req := modalModel(t)

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("z")})

	if m.pending == nil {
		t.Fatal("an unbound key dismissed the modal, want it kept")
	}
	select {
	case <-req.Reply:
		t.Fatal("an unbound key sent an answer, want none")
	default:
	}
}

func TestModel_ModalDenyKeepsTurnAlive(t *testing.T) {
	m, req := modalModel(t)

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("n")})

	select {
	case ans := <-req.Reply:
		if ans != permission.AnswerDenyOnce {
			t.Fatalf("reply = %v, want AnswerDenyOnce", ans)
		}
	default:
		t.Fatal("pressing 'n' did not send a deny answer")
	}
	if m.pending != nil {
		t.Fatal("modal still pending after deny, want it cleared")
	}
}
