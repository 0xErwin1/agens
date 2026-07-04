package tui

import (
	"errors"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"

	"github.com/iperez/agens/internal/message"
	"github.com/iperez/agens/internal/session"
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

func (f *fakeSessionStore) List() ([]session.Session, error) {
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
