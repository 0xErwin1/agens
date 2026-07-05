package tui

import (
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

// This file holds the mocked subagent playback used to see and judge the
// subagent panel before real subagents are wired in. Real delegations will drive
// the same Messages.*Subagent* lifecycle methods; when they land, this file (and
// the /subagents demo command) can be removed without touching the panel itself.

// subagentDemoDelay paces the mocked playback so each step is visible as the
// panel updates live.
const subagentDemoDelay = 700 * time.Millisecond

// subagentDemoMsg advances the mocked subagent playback to the given step.
type subagentDemoMsg struct{ step int }

// subagentDemoSteps is the scripted delegation: a top-level "explore" subagent
// that spawns a nested "grep" child, exercising the panel's metadata, live
// activity, nesting, and completion states.
func subagentDemoSteps() []func(*Messages) {
	return []func(*Messages){
		func(m *Messages) {
			m.StartSubagent("explore", "", "explore", "gpt-5.5", "high")
			m.AddSubagentActivity("explore", "reading internal/tui/*.go")
		},
		func(m *Messages) {
			m.UpdateSubagentProgress("explore", 1200, 800*time.Millisecond)
			m.StartSubagent("grep", "explore", "grep", "gpt-5.5", "")
			m.AddSubagentActivity("grep", "searching for renderTool")
		},
		func(m *Messages) {
			m.AddSubagentActivity("grep", "3 matches in messages.go")
			m.UpdateSubagentProgress("grep", 300, 400*time.Millisecond)
			m.CompleteSubagent("grep", false, 400*time.Millisecond)
		},
		func(m *Messages) {
			m.AddSubagentActivity("explore", "summarizing findings")
			m.UpdateSubagentProgress("explore", 3400, 2100*time.Millisecond)
		},
		func(m *Messages) {
			m.CompleteSubagent("explore", false, 2400*time.Millisecond)
		},
	}
}

// subagentDemoTick schedules the given playback step after the pacing delay.
func subagentDemoTick(step int) tea.Cmd {
	return tea.Tick(subagentDemoDelay, func(time.Time) tea.Msg { return subagentDemoMsg{step: step} })
}

// PlaySubagentDemo implements CommandContext: it starts the mocked subagent
// playback, noting it in the conversation and scheduling the first step.
func (m *Model) PlaySubagentDemo() tea.Cmd {
	m.messages.AddInfo("subagents (demo): playing a mocked delegation")
	return subagentDemoTick(0)
}

// advanceSubagentDemo applies one playback step and schedules the next, ending
// the playback once the script is exhausted.
func (m *Model) advanceSubagentDemo(msg subagentDemoMsg) tea.Cmd {
	steps := subagentDemoSteps()
	if msg.step < 0 || msg.step >= len(steps) {
		return nil
	}

	steps[msg.step](m.messages)

	next := msg.step + 1
	if next >= len(steps) {
		return nil
	}
	return subagentDemoTick(next)
}
