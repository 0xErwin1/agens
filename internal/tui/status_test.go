package tui

import (
	"strings"
	"testing"
)

func TestStatus_ViewShowsModelAndDefaultState(t *testing.T) {
	s := NewStatus("gpt-5.5")
	s.SetSize(80, 1)

	view := s.View()

	if !strings.Contains(view, "agens") {
		t.Fatalf("View() = %q, want it to contain %q", view, "agens")
	}
	if !strings.Contains(view, "gpt-5.5") {
		t.Fatalf("View() = %q, want it to contain the model %q", view, "gpt-5.5")
	}
	if !strings.Contains(view, "ready") {
		t.Fatalf("View() = %q, want the default state %q", view, "ready")
	}
}

func TestStatus_SetStateChangesRenderedState(t *testing.T) {
	s := NewStatus("gpt-5.5")
	s.SetSize(80, 1)

	s.SetState("thinking…")

	view := s.View()
	if !strings.Contains(view, "thinking…") {
		t.Fatalf("View() = %q, want it to contain the new state %q", view, "thinking…")
	}
	if strings.Contains(view, "ready") {
		t.Fatalf("View() = %q, must not still contain the old %q state", view, "ready")
	}
}

func TestStatus_DifferentModelAndErrorState(t *testing.T) {
	s := NewStatus("o4-mini")
	s.SetSize(80, 1)

	s.SetState("error: boom")

	view := s.View()
	if !strings.Contains(view, "o4-mini") {
		t.Fatalf("View() = %q, want it to contain the model %q", view, "o4-mini")
	}
	if !strings.Contains(view, "error: boom") {
		t.Fatalf("View() = %q, want it to contain the error state", view)
	}
}

func TestStatus_ThemedViewKeepsModelAndStateContent(t *testing.T) {
	s := NewStatus("gpt-5.5")
	s.SetSize(80, 1)
	s.SetState("thinking…")

	view := s.View()
	if strings.TrimSpace(view) == "" {
		t.Fatal("View() is empty, want a rendered themed status line")
	}

	stripped := stripANSI(view)
	if !strings.Contains(stripped, "agens") {
		t.Fatalf("stripped View() = %q, want the accented app name %q", stripped, "agens")
	}
	if !strings.Contains(stripped, "gpt-5.5") {
		t.Fatalf("stripped View() = %q, want the model segment %q", stripped, "gpt-5.5")
	}
	if !strings.Contains(stripped, "thinking…") {
		t.Fatalf("stripped View() = %q, want the state segment %q", stripped, "thinking…")
	}
}
