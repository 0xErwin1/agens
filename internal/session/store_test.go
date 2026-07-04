package session

import (
	"path/filepath"
	"testing"
	"time"

	"github.com/iperez/agens/internal/message"
)

func TestStore_SaveListLoadRoundTrip(t *testing.T) {
	store := NewStore(t.TempDir())

	sess := Session{
		ID:      "abc",
		Title:   "first chat",
		Project: "/home/me/projA",
		Messages: []message.Message{
			message.NewMessage(message.RoleUser, message.TextPart{Text: "hola"}),
			message.NewMessage(message.RoleAssistant, message.TextPart{Text: "buenas"}),
		},
	}
	if err := store.Save(sess); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	loaded, err := store.Load("abc")
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if loaded.Title != "first chat" || len(loaded.Messages) != 2 {
		t.Fatalf("loaded = %+v, want title and 2 messages preserved", loaded)
	}
	if loaded.Project != "/home/me/projA" {
		t.Fatalf("loaded project = %q, want it preserved across save/load", loaded.Project)
	}
	if loaded.Updated.IsZero() {
		t.Fatal("Save did not stamp Updated")
	}

	list, err := store.List()
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(list) != 1 || list[0].ID != "abc" {
		t.Fatalf("List() = %+v, want the one saved session", list)
	}
}

func TestStore_ListSortsByUpdatedDescending(t *testing.T) {
	store := NewStore(t.TempDir())

	for _, id := range []string{"a", "b", "c"} {
		if err := store.Save(Session{ID: id, Title: id}); err != nil {
			t.Fatalf("Save(%s) error = %v", id, err)
		}
		time.Sleep(2 * time.Millisecond) // ensure distinct Updated stamps
	}

	list, err := store.List()
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(list) != 3 || list[0].ID != "c" || list[2].ID != "a" {
		t.Fatalf("List() order = %v, want most-recent (c) first", ids(list))
	}
}

func TestStore_ListMissingDirIsEmpty(t *testing.T) {
	store := NewStore(filepath.Join(t.TempDir(), "does-not-exist"))

	list, err := store.List()
	if err != nil {
		t.Fatalf("List() error = %v, want nil for a missing dir", err)
	}
	if len(list) != 0 {
		t.Fatalf("List() = %v, want empty", list)
	}
}

func TestStore_SaveRequiresID(t *testing.T) {
	if err := NewStore(t.TempDir()).Save(Session{}); err == nil {
		t.Fatal("Save() with no ID returned nil, want an error")
	}
}

func ids(sessions []Session) []string {
	out := make([]string, len(sessions))
	for i, s := range sessions {
		out[i] = s.ID
	}
	return out
}
