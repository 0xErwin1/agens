package tui

import (
	"context"
	"encoding/json"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/permission"
)

// Modal geometry. The permission modal replaces the input area while it is
// shown; modalHeight is reserved by the layout so the messages view shrinks
// by exactly the modal's height rather than the input's, keeping the whole
// frame within the terminal. Every content line is rendered on a single row
// (Inline + MaxWidth), so the rendered height is deterministic.
const (
	modalContentRows = 4
	modalHeight      = modalContentRows + 2 // + rounded border (top and bottom)
)

// PermissionRequest is one pending tool-permission decision handed from the
// agent loop's goroutine to the Bubble Tea event loop. Reply is a buffered
// channel the model sends the chosen Answer back on.
type PermissionRequest struct {
	Call  message.ToolUsePart
	Reply chan permission.Answer
}

// Prompter is the permission.Prompter the TUI installs into the agent loop.
// Unlike a terminal prompter, it never reads the tty itself — doing so would
// fight Bubble Tea, which owns the terminal — instead it forwards each Ask
// decision onto an unbuffered channel the root model drains, then blocks
// until the model replies or ctx is canceled.
type Prompter struct {
	requests chan PermissionRequest
}

// NewPrompter returns a Prompter whose Requests channel the root model must
// listen on for the prompt to ever resolve.
func NewPrompter() *Prompter {
	return &Prompter{requests: make(chan PermissionRequest)}
}

var _ permission.Prompter = (*Prompter)(nil)

// Requests exposes the request channel for the model to listen on.
func (p *Prompter) Requests() <-chan PermissionRequest { return p.requests }

// Prompt forwards call onto the request channel and blocks for the model's
// answer. Both the handoff and the wait honor ctx: a cancellation returns
// AnswerCancel with ctx.Err(), satisfying the permission.Prompter contract
// that ctx be honored promptly even while a human is deciding.
func (p *Prompter) Prompt(ctx context.Context, call message.ToolUsePart) (permission.Answer, error) {
	reply := make(chan permission.Answer, 1)
	req := PermissionRequest{Call: call, Reply: reply}

	select {
	case p.requests <- req:
	case <-ctx.Done():
		return permission.AnswerCancel, ctx.Err()
	}

	select {
	case answer := <-reply:
		return answer, nil
	case <-ctx.Done():
		return permission.AnswerCancel, ctx.Err()
	}
}

// PermissionRequestMsg delivers a PermissionRequest into the model's Update.
type PermissionRequestMsg struct {
	Request PermissionRequest
}

// waitForPermission returns a tea.Cmd that blocks on the next permission
// request and wraps it in a PermissionRequestMsg. It returns nil when ch is
// nil (no interactive prompter installed, e.g. --dangerously-allow-all) so it
// is safe to Batch unconditionally.
func waitForPermission(ch <-chan PermissionRequest) tea.Cmd {
	if ch == nil {
		return nil
	}
	return func() tea.Msg {
		req, ok := <-ch
		if !ok {
			return nil
		}
		return PermissionRequestMsg{Request: req}
	}
}

// answerForModalKey maps a key pressed while the permission modal is open to a
// permission.Answer. It reports ok=false for keys the modal does not bind, so
// the caller can ignore them rather than resolve the prompt. Escape is treated
// as a deny-once, keeping the turn alive so the model can react to the denial.
func answerForModalKey(msg tea.KeyMsg) (permission.Answer, bool) {
	if msg.Type == tea.KeyEsc {
		return permission.AnswerDenyOnce, true
	}
	switch strings.ToLower(msg.String()) {
	case "y":
		return permission.AnswerAllowOnce, true
	case "a":
		return permission.AnswerAllowAlways, true
	case "n":
		return permission.AnswerDenyOnce, true
	case "d":
		return permission.AnswerDenyAlways, true
	default:
		return permission.AnswerDenyOnce, false
	}
}

// permissionArgs is the best-effort shape used to pull a human-readable detail
// out of a tool call's input: the shell command, the file path, or the URL,
// whichever is present.
type permissionArgs struct {
	Command string `json:"command"`
	Path    string `json:"path"`
	URL     string `json:"url"`
}

// permissionDetail returns a short one-line description of what the tool call
// will act on, or the empty string when the input carries nothing worth
// showing.
func permissionDetail(input json.RawMessage) string {
	var a permissionArgs
	if err := json.Unmarshal(input, &a); err == nil {
		switch {
		case a.Command != "":
			return a.Command
		case a.Path != "":
			return a.Path
		case a.URL != "":
			return a.URL
		}
	}

	raw := strings.TrimSpace(string(input))
	if raw == "" || raw == "{}" || raw == "null" {
		return ""
	}
	return raw
}

// renderPermission renders the permission modal for call at the given width.
// Each line is forced onto a single row so the box is always exactly
// modalHeight rows tall, which the layout has already reserved.
func renderPermission(call message.ToolUsePart, width int) string {
	theme := CurrentTheme()

	inner := width - 4 // border (2) + horizontal padding (2)
	if inner < 8 {
		inner = 8
	}

	oneLine := func(s string) string {
		return lipgloss.NewStyle().Inline(true).MaxWidth(inner).Render(s)
	}

	title := oneLine(lipgloss.NewStyle().Foreground(theme.Accent()).Bold(true).Render("Permission required"))

	action := lipgloss.NewStyle().Foreground(theme.Tool()).Render("→ " + call.Name)
	if detail := permissionDetail(call.Input); detail != "" {
		action += "  " + lipgloss.NewStyle().Foreground(theme.Muted()).Render(detail)
	}
	action = oneLine(action)

	hint := oneLine(lipgloss.NewStyle().Foreground(theme.Muted()).Render(
		"y allow · a always · n deny · d deny-all · esc skip"))

	content := strings.Join([]string{title, action, "", hint}, "\n")

	box := lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		BorderForeground(theme.Accent()).
		Padding(0, 1).
		Height(modalContentRows).
		MaxHeight(modalContentRows)
	if width > 4 {
		box = box.Width(width - 2)
	}

	return box.Render(content)
}
