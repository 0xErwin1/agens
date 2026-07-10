package tui

import (
	"context"
	"errors"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/provider"
)

// fakeLister is a ModelLister double returning a fixed catalog or error.
type fakeLister struct {
	models []provider.ModelInfo
	err    error
}

func (f fakeLister) Models(context.Context) ([]provider.ModelInfo, error) {
	return f.models, f.err
}

func sizedWithLister(lister ModelLister) *Model {
	m := New(Deps{
		Loop:         &scriptedLoopRunner{},
		Model:        "gpt-5.5",
		Models:       lister,
		SystemPrompt: func(id string) (string, bool) { return "You are powered by the model named " + id, true },
	})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})
	return m
}

func openModelSelector(t *testing.T, m *Model) {
	t.Helper()
	typeString(m, "/model")
	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})
	if cmd == nil {
		t.Fatal("/model did not return a command to load the catalog")
	}
	// Deliver the async load result synchronously.
	m.Update(cmd())
}

func TestModel_ModelSelectorLoadsAndSwitches(t *testing.T) {
	lister := fakeLister{models: []provider.ModelInfo{
		{ID: "gpt-5.5"},
		{ID: "gpt-5.4"},
		{ID: "gpt-5.4-mini"},
	}}
	m := sizedWithLister(lister)

	openModelSelector(t, m)

	if !m.modelPickerOpen {
		t.Fatal("model selector did not open")
	}
	if m.modelLoading {
		t.Fatal("selector still loading after the catalog arrived")
	}
	// Opens on the active model.
	if m.modelName != "gpt-5.5" || m.modelIdx != 0 {
		t.Fatalf("selector opened on idx %d (%s), want the active model gpt-5.5 at 0", m.modelIdx, m.modelName)
	}

	view := stripANSI(m.View())
	if !strings.Contains(view, "gpt-5.4-mini") {
		t.Fatalf("View() = %q, want the model list rendered", view)
	}

	// Move to gpt-5.4 and select it.
	sendKey(m, tea.KeyMsg{Type: tea.KeyDown})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.modelPickerOpen {
		t.Fatal("selector still open after choosing a model")
	}
	if m.modelName != "gpt-5.4" {
		t.Fatalf("modelName = %q, want it switched to gpt-5.4", m.modelName)
	}
	runner, ok := m.loop.(*scriptedLoopRunner)
	if !ok || runner.model != "gpt-5.4" {
		t.Fatal("SetModel was not propagated to the loop")
	}
	if !strings.Contains(runner.systemPrompt, "gpt-5.4") {
		t.Fatalf("system prompt = %q, want it rebuilt for the new model gpt-5.4", runner.systemPrompt)
	}
	if !strings.Contains(stripANSI(m.status.View()), "gpt-5.4") {
		t.Fatal("status bar was not updated to the new model")
	}
}

func TestModel_ModelSelectorTabCyclesWithWrap(t *testing.T) {
	m := sizedWithLister(fakeLister{models: []provider.ModelInfo{{ID: "a"}, {ID: "b"}}})
	openModelSelector(t, m)

	sendKey(m, tea.KeyMsg{Type: tea.KeyTab})
	if m.modelIdx != 1 {
		t.Fatalf("after Tab idx = %d, want 1", m.modelIdx)
	}
	sendKey(m, tea.KeyMsg{Type: tea.KeyTab}) // wraps
	if m.modelIdx != 0 {
		t.Fatalf("after wrap idx = %d, want 0", m.modelIdx)
	}
	sendKey(m, tea.KeyMsg{Type: tea.KeyShiftTab}) // wraps back to last
	if m.modelIdx != 1 {
		t.Fatalf("after Shift+Tab wrap idx = %d, want 1", m.modelIdx)
	}
}

func TestModel_ModelSelectorEscClosesWithoutChange(t *testing.T) {
	m := sizedWithLister(fakeLister{models: []provider.ModelInfo{{ID: "gpt-5.5"}, {ID: "gpt-5.4"}}})
	openModelSelector(t, m)

	sendKey(m, tea.KeyMsg{Type: tea.KeyDown})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEsc})

	if m.modelPickerOpen {
		t.Fatal("Esc did not close the selector")
	}
	if m.modelName != "gpt-5.5" {
		t.Fatalf("modelName = %q, want it unchanged after Esc", m.modelName)
	}
}

func TestModel_ModelSelectorShowsLoadError(t *testing.T) {
	m := sizedWithLister(fakeLister{err: errors.New("backend down")})
	openModelSelector(t, m)

	view := stripANSI(m.View())
	if !strings.Contains(view, "backend down") {
		t.Fatalf("View() = %q, want the load error shown", view)
	}
}

func TestModelRows_ShowsPriceSuffixForKnownPricingAndHidesItForNil(t *testing.T) {
	inCost := 0.15
	outCost := 0.6
	items := []provider.ModelInfo{
		{ID: "gpt-4o-mini", InputCostPerMTok: &inCost, OutputCostPerMTok: &outCost},
		{ID: "custom-model"},
	}

	rows := modelRows(items, 0, "", CurrentTheme(), 80)
	if len(rows) != 2 {
		t.Fatalf("modelRows returned %d rows, want 2", len(rows))
	}

	if got := stripANSI(rows[0]); !strings.Contains(got, "$0.15/$0.60") {
		t.Fatalf("row[0] = %q, want the formatted price for gpt-4o-mini", got)
	}
	if got := stripANSI(rows[1]); strings.Contains(got, "$0.00") || strings.Contains(got, "$0") {
		t.Fatalf("row[1] = %q, want no $0 price rendered for nil pricing", got)
	}
}

func TestWindowStart(t *testing.T) {
	cases := []struct{ selected, total, size, want int }{
		{0, 3, 8, 0},
		{5, 20, 8, 0},   // selection still within first window
		{8, 20, 8, 1},   // scrolls by one to keep 8 visible
		{19, 20, 8, 12}, // clamped to the end
	}
	for _, c := range cases {
		if got := windowStart(c.selected, c.total, c.size); got != c.want {
			t.Fatalf("windowStart(%d,%d,%d) = %d, want %d", c.selected, c.total, c.size, got, c.want)
		}
	}
}
