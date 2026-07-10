package sessiondb

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"reflect"
	"sync"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/session"
)

func TestStore_SaveListLoadRoundTrip(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "sessions.db"))

	want := session.Session{
		ID:      "abc",
		Title:   "first chat",
		Project: "/home/me/projA",
		Agent:   "main-agent",
		Messages: []message.Message{
			{
				ID:        "user-1",
				Role:      message.RoleUser,
				Parts:     message.Parts{message.TextPart{Text: "hola"}},
				CreatedAt: time.Date(2026, 7, 8, 10, 0, 0, 0, time.UTC),
			},
			{
				ID:   "assistant-1",
				Role: message.RoleAssistant,
				Parts: message.Parts{
					message.TextPart{Text: "checking"},
					message.ToolUsePart{ID: "tool-1", Name: "search", Input: json.RawMessage(`{"query":"sqlite"}`)},
				},
				Model:      "gpt-test",
				StopReason: "tool_use",
				CreatedAt:  time.Date(2026, 7, 8, 10, 0, 1, 0, time.UTC),
			},
			{
				ID:        "user-2",
				Role:      message.RoleUser,
				Parts:     message.Parts{message.ToolResultPart{ToolUseID: "tool-1", Content: message.Parts{message.TextPart{Text: "result"}}, IsError: true}},
				CreatedAt: time.Date(2026, 7, 8, 10, 0, 2, 0, time.UTC),
			},
		},
	}
	if err := store.Save(want); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	got, err := store.Load("abc")
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}
	if got.ID != want.ID || got.Title != want.Title || got.Project != want.Project || got.Agent != want.Agent {
		t.Fatalf("loaded metadata = %+v, want %+v", got, want)
	}
	if got.Updated.IsZero() || got.Updated.Location() != time.UTC {
		t.Fatalf("Updated = %v, want non-zero UTC stamp", got.Updated)
	}
	if !reflect.DeepEqual(got.Messages, want.Messages) {
		t.Fatalf("Messages = %#v, want %#v", got.Messages, want.Messages)
	}

	list, err := store.List()
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(list) != 1 || list[0].ID != "abc" || list[0].Project != "/home/me/projA" || !reflect.DeepEqual(list[0].Messages, want.Messages) {
		t.Fatalf("List() = %+v, want saved session with messages and project", list)
	}
}

func TestStore_ListSortsByUpdatedDescending(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "sessions.db"))

	for _, id := range []string{"a", "b", "c"} {
		if err := store.Save(session.Session{ID: id, Title: id}); err != nil {
			t.Fatalf("Save(%s) error = %v", id, err)
		}
		time.Sleep(2 * time.Millisecond)
	}

	list, err := store.List()
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(list) != 3 || list[0].ID != "c" || list[2].ID != "a" {
		t.Fatalf("List() order = %v, want most-recent (c) first", ids(list))
	}
}

func TestStore_ConcurrentSavesAreSerialized(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "sessions.db"))

	const total = 20
	var wg sync.WaitGroup
	errs := make(chan error, total)
	for i := 0; i < total; i++ {
		i := i
		wg.Add(1)
		go func() {
			defer wg.Done()
			err := store.Save(session.Session{
				ID:    string(rune('a' + i)),
				Title: "concurrent",
				Messages: []message.Message{
					{ID: "msg", Role: message.RoleUser, Parts: message.Parts{message.TextPart{Text: "hello"}}, CreatedAt: time.Unix(int64(i), 0).UTC()},
				},
			})
			errs <- err
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatalf("Save() error = %v", err)
		}
	}

	list, err := store.List()
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(list) != total {
		t.Fatalf("len(List()) = %d, want %d", len(list), total)
	}
}

func TestDefaultPathUsesXDGDataHome(t *testing.T) {
	dataHome := t.TempDir()
	t.Setenv("XDG_DATA_HOME", dataHome)

	want := filepath.Join(dataHome, "agens", "sessions.db")
	if got := DefaultPath(); got != want {
		t.Fatalf("DefaultPath() = %q, want %q", got, want)
	}
}

func TestOpenCreatesDatabaseOnFirstSave(t *testing.T) {
	path := filepath.Join(t.TempDir(), "nested", "sessions.db")
	store := openTestStore(t, path)

	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("database exists before first save or stat failed: %v", err)
	}
	if err := store.Save(session.Session{ID: "abc", Title: "created"}); err != nil {
		t.Fatalf("Save() error = %v", err)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("database was not created at %q: %v", path, err)
	}
}

func TestStore_SaveRequiresID(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "sessions.db"))
	if err := store.Save(session.Session{}); err == nil {
		t.Fatal("Save() with no ID returned nil, want an error")
	}
}

func TestStore_ListMetaReturnsNoMessages(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "sessions.db"))

	for _, id := range []string{"a", "b"} {
		sess := session.Session{
			ID:      id,
			Title:   id,
			Project: "/home/me/projA",
			Messages: []message.Message{
				{ID: "msg", Role: message.RoleUser, Parts: message.Parts{message.TextPart{Text: "hi"}}, CreatedAt: time.Now().UTC()},
			},
		}
		if err := store.Save(sess); err != nil {
			t.Fatalf("Save(%s) error = %v", id, err)
		}
		time.Sleep(2 * time.Millisecond)
	}

	metas, err := store.ListMeta()
	if err != nil {
		t.Fatalf("ListMeta() error = %v", err)
	}
	if len(metas) != 2 || metas[0].ID != "b" || metas[1].ID != "a" {
		t.Fatalf("ListMeta() order = %v, want most-recent (b) first", ids(metas))
	}
	for _, meta := range metas {
		if meta.Messages != nil {
			t.Fatalf("ListMeta() session %q has Messages = %#v, want nil", meta.ID, meta.Messages)
		}
		if meta.Project != "/home/me/projA" {
			t.Fatalf("ListMeta() session %q Project = %q, want %q", meta.ID, meta.Project, "/home/me/projA")
		}
	}
}

func TestStore_DeleteCascadesMessages(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "sessions.db"))

	sess := session.Session{
		ID:    "abc",
		Title: "to delete",
		Messages: []message.Message{
			{ID: "msg", Role: message.RoleUser, Parts: message.Parts{message.TextPart{Text: "hi"}}, CreatedAt: time.Now().UTC()},
		},
	}
	if err := store.Save(sess); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	if err := store.Delete("abc"); err != nil {
		t.Fatalf("Delete() error = %v", err)
	}

	if _, err := store.Load("abc"); err == nil {
		t.Fatal("Load() after Delete() returned nil error, want not-found")
	}
	metas, err := store.ListMeta()
	if err != nil {
		t.Fatalf("ListMeta() error = %v", err)
	}
	if len(metas) != 0 {
		t.Fatalf("ListMeta() after Delete() = %v, want empty", ids(metas))
	}

	var messageCount int
	row := store.db.QueryRow("SELECT COUNT(*) FROM messages WHERE session_id = ?", "abc")
	if err := row.Scan(&messageCount); err != nil {
		t.Fatalf("count messages: %v", err)
	}
	if messageCount != 0 {
		t.Fatalf("messages remaining for deleted session = %d, want 0", messageCount)
	}
}

func TestStore_DeleteMissingIDIsNoop(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "sessions.db"))

	if err := store.Delete("does-not-exist"); err != nil {
		t.Fatalf("Delete() of missing id error = %v, want nil", err)
	}
}

func openTestStore(t *testing.T, path string) *Store {
	t.Helper()
	store, err := Open(path)
	if err != nil {
		t.Fatalf("Open() error = %v", err)
	}
	t.Cleanup(func() {
		if err := store.Close(); err != nil {
			t.Fatalf("Close() error = %v", err)
		}
	})
	return store
}

func ids(sessions []session.Session) []string {
	out := make([]string, len(sessions))
	for i, s := range sessions {
		out[i] = s.ID
	}
	return out
}
