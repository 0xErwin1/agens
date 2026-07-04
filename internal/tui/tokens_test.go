package tui

import (
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/agentloop"
	"github.com/iperez/agens/internal/provider"
)

// Reuses fakeLister and scriptedLoopRunner from the package's other tests.

func TestFormatTokens(t *testing.T) {
	cases := map[int]string{
		0:       "0",
		823:     "823",
		1500:    "1.5K",
		141100:  "141.1K",
		1250000: "1.2M",
	}
	for n, want := range cases {
		if got := formatTokens(n); got != want {
			t.Fatalf("formatTokens(%d) = %q, want %q", n, got, want)
		}
	}
}

func TestModel_FooterShowsTokensAndContextPercent(t *testing.T) {
	lister := fakeLister{models: []provider.ModelInfo{
		{ID: "gpt-5.5", ContextWindow: 400000},
	}}
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Models: lister})
	m.Update(tea.WindowSizeMsg{Width: 120, Height: 24})

	// Deliver the background catalog so the context window is known.
	m.Update(modelsLoadedMsg{models: lister.models})

	// A usage report of 140K in + 1.1K out = 141.1K of a 400K window ≈ 35%.
	usage := &provider.Usage{InputTokens: 140000, OutputTokens: 1100}
	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopUsage, Usage: usage}})

	view := stripANSI(m.status.View())
	if !strings.Contains(view, "141.1K") {
		t.Fatalf("status View() = %q, want the token total", view)
	}
	if !strings.Contains(view, "35%") {
		t.Fatalf("status View() = %q, want the context-window percentage", view)
	}
}

func TestModel_CtrlPTogglesDetailedTokens(t *testing.T) {
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5"})
	m.Update(tea.WindowSizeMsg{Width: 120, Height: 24})

	usage := &provider.Usage{InputTokens: 140000, OutputTokens: 1100}
	sendMsg(m, StreamMsg{Event: agentloop.LoopEvent{Kind: agentloop.LoopUsage, Usage: usage}})

	// Compact by default: the combined total, no in/out split.
	if got := stripANSI(m.status.View()); !strings.Contains(got, "141.1K") || strings.Contains(got, "in ") {
		t.Fatalf("status View() = %q, want the compact token total", got)
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlP})

	view := stripANSI(m.status.View())
	if !strings.Contains(view, "in 140.0K") || !strings.Contains(view, "out 1.1K") {
		t.Fatalf("status View() = %q, want the detailed in/out token breakdown after Ctrl+P", view)
	}
}
