package tui

import (
	"errors"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/session"
)

// fakeSessionStore is a SessionStore double backed by an in-memory map.
type fakeSessionStore struct {
	saved    []session.Session
	list     []session.Session
	listErr  error
	loadByID map[string]session.Session
}

func (f *fakeSessionStore) Save(s session.Session) error {
	f.saved = append(f.saved, s)
	return nil
}

func (f *fakeSessionStore) ListMeta() ([]session.Session, error) {
	return f.list, f.listErr
}

func (f *fakeSessionStore) Load(id string) (session.Session, error) {
	if s, ok := f.loadByID[id]; ok {
		return s, nil
	}
	return session.Session{}, errors.New("not found")
}

func sizedWithSessions(store SessionStore) *Model {
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Sessions: store})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})
	return m
}

func TestModel_SavesSessionOnTurnDone(t *testing.T) {
	store := &fakeSessionStore{}
	m := sizedWithSessions(store)

	m.history = []message.Message{
		message.NewMessage(message.RoleUser, message.TextPart{Text: "a Go question"}),
	}
	m.handleDone(TurnDoneMsg{History: m.history})

	if len(store.saved) != 1 {
		t.Fatalf("saved %d sessions, want 1", len(store.saved))
	}
	if store.saved[0].ID != m.sessionID {
		t.Fatalf("saved ID = %q, want the current session %q", store.saved[0].ID, m.sessionID)
	}
	if store.saved[0].Title != "a Go question" {
		t.Fatalf("saved title = %q, want it derived from the first user message", store.saved[0].Title)
	}
}

func TestModel_DoesNotSaveEmptyOrCanceled(t *testing.T) {
	store := &fakeSessionStore{}
	m := sizedWithSessions(store)

	// No history → nothing to save.
	m.handleDone(TurnDoneMsg{})
	if len(store.saved) != 0 {
		t.Fatalf("saved %d sessions for an empty history, want 0", len(store.saved))
	}
}

func TestModel_NewConversationStartsNewSession(t *testing.T) {
	m := sizedWithSessions(&fakeSessionStore{})
	first := m.sessionID

	m.history = []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "x"})}
	m.NewConversation()

	if m.sessionID == first {
		t.Fatal("NewConversation kept the same session id, want a fresh one")
	}
	if len(m.history) != 0 {
		t.Fatal("NewConversation did not clear the history")
	}
}

func TestModel_SessionPickerResumesConversation(t *testing.T) {
	past := session.Session{
		ID:    "old",
		Title: "past chat",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser, message.TextPart{Text: "earlier question"}),
			message.NewMessage(message.RoleAssistant, message.TextPart{Text: "earlier answer"}),
		},
	}
	store := &fakeSessionStore{
		list:     []session.Session{past},
		loadByID: map[string]session.Session{"old": past},
	}
	m := sizedWithSessions(store)

	typeString(m, "/sessions")
	cmd := sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // run /sessions → returns list command
	if cmd == nil {
		t.Fatal("/sessions did not return a command to list conversations")
	}
	m.Update(cmd()) // deliver the loaded list

	if !m.sessionPickerOpen {
		t.Fatal("session picker did not open")
	}
	if !strings.Contains(stripANSI(m.View()), "past chat") {
		t.Fatalf("View() = %q, want the saved conversation listed", stripANSI(m.View()))
	}

	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter}) // resume the selected session

	if m.sessionPickerOpen {
		t.Fatal("picker still open after resuming")
	}
	if m.sessionID != "old" {
		t.Fatalf("sessionID = %q, want the resumed session %q", m.sessionID, "old")
	}
	if len(m.history) != 2 {
		t.Fatalf("history has %d messages, want the resumed 2", len(m.history))
	}
	if !strings.Contains(stripANSI(m.View()), "earlier question") {
		t.Fatalf("View() = %q, want the resumed conversation rendered", stripANSI(m.View()))
	}
}

func TestModel_SavesSessionWithProject(t *testing.T) {
	store := &fakeSessionStore{}
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Sessions: store, Project: "/home/me/projA"})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	m.history = []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "q"})}
	m.handleDone(TurnDoneMsg{History: m.history})

	if len(store.saved) != 1 {
		t.Fatalf("saved %d sessions, want 1", len(store.saved))
	}
	if store.saved[0].Project != "/home/me/projA" {
		t.Fatalf("saved project = %q, want the current project root", store.saved[0].Project)
	}
}

func TestModel_SessionPickerFiltersByProjectAndTogglesAll(t *testing.T) {
	sessions := []session.Session{
		{ID: "a", Title: "this-project chat", Project: "/home/me/projA"},
		{ID: "b", Title: "other-project chat", Project: "/home/me/projB"},
		{ID: "c", Title: "legacy chat"}, // no project
	}
	store := &fakeSessionStore{list: sessions}
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Sessions: store, Project: "/home/me/projA"})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	cmd := m.OpenSessionPicker()
	m.Update(cmd()) // deliver the full list

	// Default scope: only the current project's session is listed.
	if len(m.sessionItems) != 1 || m.sessionItems[0].ID != "a" {
		t.Fatalf("default picker items = %v, want only the current project's session", m.sessionItems)
	}
	view := stripANSI(m.View())
	if !strings.Contains(view, "this-project chat") || strings.Contains(view, "other-project chat") {
		t.Fatalf("View() = %q, want only this project's conversation by default", view)
	}

	// Ctrl+A widens to every project.
	sendKey(m, tea.KeyMsg{Type: tea.KeyCtrlA})

	if len(m.sessionItems) != 3 {
		t.Fatalf("after ctrl+a, picker items = %d, want all 3 across projects", len(m.sessionItems))
	}
	view = stripANSI(m.View())
	if !strings.Contains(view, "other-project chat") || !strings.Contains(view, "projB") {
		t.Fatalf("View() = %q, want all-projects view tagging each session's project", view)
	}
}

func TestModel_ResumeIDOnStartupLoadsSession(t *testing.T) {
	past := session.Session{
		ID:       "old",
		Title:    "past chat",
		Messages: []message.Message{message.NewMessage(message.RoleUser, message.TextPart{Text: "earlier question"})},
	}
	store := &fakeSessionStore{loadByID: map[string]session.Session{"old": past}}
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Sessions: store, ResumeID: "old"})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	// Init wires the resume command; run the batch and deliver its messages.
	for _, msg := range runCmd(m.Init()) {
		m.Update(msg)
	}

	if m.sessionID != "old" {
		t.Fatalf("sessionID = %q, want the resumed session %q", m.sessionID, "old")
	}
	if len(m.history) != 1 {
		t.Fatalf("history has %d messages, want the resumed 1", len(m.history))
	}
	if !strings.Contains(stripANSI(m.View()), "earlier question") {
		t.Fatalf("View() = %q, want the resumed conversation rendered", stripANSI(m.View()))
	}
}

func TestModel_OpenSessionsOnStartupOpensPicker(t *testing.T) {
	store := &fakeSessionStore{list: []session.Session{{ID: "a", Title: "a chat"}}}
	m := New(Deps{Loop: &scriptedLoopRunner{}, Model: "gpt-5.5", Sessions: store, OpenSessions: true})
	m.Update(tea.WindowSizeMsg{Width: 80, Height: 24})

	m.Init()

	if !m.sessionPickerOpen {
		t.Fatal("session picker did not open on startup with OpenSessions set")
	}
}

// runCmd executes a tea.Cmd, flattening a batch into the messages its children
// produce so a startup batch can be delivered back into Update.
func runCmd(cmd tea.Cmd) []tea.Msg {
	if cmd == nil {
		return nil
	}
	msg := cmd()
	batch, ok := msg.(tea.BatchMsg)
	if !ok {
		return []tea.Msg{msg}
	}
	var msgs []tea.Msg
	for _, c := range batch {
		if c == nil {
			continue
		}
		msgs = append(msgs, c())
	}
	return msgs
}

func TestModel_SessionsUnavailableWithoutStore(t *testing.T) {
	m := sized(&scriptedLoopRunner{}, "gpt-5.5") // no store

	typeString(m, "/sessions")
	sendKey(m, tea.KeyMsg{Type: tea.KeyEnter})

	if m.sessionPickerOpen {
		t.Fatal("picker opened without a store")
	}
	if !strings.Contains(stripANSI(m.View()), "not available") {
		t.Fatalf("View() = %q, want an unavailable note", stripANSI(m.View()))
	}
}
