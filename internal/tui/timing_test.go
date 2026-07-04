package tui

import (
	"strings"
	"testing"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

func TestFormatDuration(t *testing.T) {
	cases := map[time.Duration]string{
		1500 * time.Millisecond: "1.5s",
		9900 * time.Millisecond: "9.9s",
		10 * time.Second:        "10s",
		42 * time.Second:        "42s",
		63 * time.Second:        "1m03s",
		125 * time.Second:       "2m05s",
	}
	for d, want := range cases {
		if got := formatDuration(d); got != want {
			t.Fatalf("formatDuration(%s) = %q, want %q", d, got, want)
		}
	}
}

// fakeClock returns times from a slice on successive calls, holding the last
// value once exhausted, so a test can script the turn's start and end.
func fakeClock(times ...time.Time) func() time.Time {
	i := 0
	return func() time.Time {
		t := times[i]
		if i < len(times)-1 {
			i++
		}
		return t
	}
}

func TestModel_WorkingLineShowsLiveElapsed(t *testing.T) {
	start := time.Unix(1000, 0)
	m := New(Deps{
		Loop:  &scriptedLoopRunner{},
		Model: "gpt-5.5",
		Now:   fakeClock(start, start.Add(3*time.Second)),
	})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // submit stamps turnStart = start

	// The next now() is start+3s, so the inline working line shows the elapsed.
	if got := stripANSI(m.workingLine()); !strings.Contains(got, "3.0s") {
		t.Fatalf("workingLine() = %q, want the live elapsed counter", got)
	}
}

func TestModel_FooterShowsTurnDurationWhenDone(t *testing.T) {
	start := time.Unix(2000, 0)
	m := New(Deps{
		Loop:  &scriptedLoopRunner{},
		Model: "gpt-5.5",
		Now:   fakeClock(start, start.Add(4200*time.Millisecond)),
	})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // turnStart = start

	m.handleDone(TurnDoneMsg{}) // now() = start+4.2s

	view := stripANSI(m.status.View())
	if !strings.Contains(view, "4.2s") {
		t.Fatalf("status View() = %q, want the completed turn's duration", view)
	}
	if !strings.Contains(view, "ready") {
		t.Fatalf("status View() = %q, want the ready state alongside the duration", view)
	}
}

func TestModel_NewTurnClearsPreviousDuration(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5")
	m.status.SetDuration("4.2s")

	sendKey(m, tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune("hi")})
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // submit clears the stale duration

	if strings.Contains(stripANSI(m.status.View()), "4.2s") {
		t.Fatalf("status View() still shows the previous turn's duration after a new submit")
	}
}
