package cli

import (
	"bytes"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/session"
)

// sessionsFakeStore is a minimal sessionStore for sessions command tests:
// ListMeta and Delete return scripted results, Close is a no-op.
type sessionsFakeStore struct {
	sessions  []session.Session
	listErr   error
	deleteErr error
	deletedID string
	closed    bool
}

var _ sessionStore = (*sessionsFakeStore)(nil)

func (f *sessionsFakeStore) ListMeta() ([]session.Session, error) {
	return f.sessions, f.listErr
}

func (f *sessionsFakeStore) Delete(id string) error {
	f.deletedID = id
	return f.deleteErr
}

func (f *sessionsFakeStore) Close() error {
	f.closed = true
	return nil
}

func openerFor(store *sessionsFakeStore, project string) sessionStoreOpener {
	return func() (sessionStore, string, error) {
		return store, project, nil
	}
}

func TestSessionsListCommand_ScopesToCurrentProjectByDefault(t *testing.T) {
	now := time.Now()
	fake := &sessionsFakeStore{
		sessions: []session.Session{
			{ID: "s1", Title: "here", Project: "/repo/a", Agent: "build", Updated: now},
			{ID: "s2", Title: "elsewhere", Project: "/repo/b", Agent: "build", Updated: now},
		},
	}

	cmd := newSessionsCommandWithOpener(openerFor(fake, "/repo/a"))
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"list"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	got := out.String()
	if !strings.Contains(got, "here") {
		t.Fatalf("stdout = %q, want the current-project session listed", got)
	}
	if strings.Contains(got, "elsewhere") {
		t.Fatalf("stdout = %q, want the other-project session excluded by default", got)
	}
	if !fake.closed {
		t.Fatal("want the store closed after the command runs")
	}
}

func TestSessionsListCommand_AllFlagCrossesProjects(t *testing.T) {
	now := time.Now()
	fake := &sessionsFakeStore{
		sessions: []session.Session{
			{ID: "s1", Title: "here", Project: "/repo/a", Agent: "build", Updated: now},
			{ID: "s2", Title: "elsewhere", Project: "/repo/b", Agent: "build", Updated: now},
		},
	}

	cmd := newSessionsCommandWithOpener(openerFor(fake, "/repo/a"))
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"list", "--all"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}

	got := out.String()
	for _, want := range []string{"here", "elsewhere"} {
		if !strings.Contains(got, want) {
			t.Fatalf("stdout = %q, want it to contain %q with --all", got, want)
		}
	}
}

func TestSessionsListCommand_EmptySetPrintsMessage(t *testing.T) {
	fake := &sessionsFakeStore{sessions: nil}

	cmd := newSessionsCommandWithOpener(openerFor(fake, "/repo/a"))
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"list"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if got, want := out.String(), "No saved sessions.\n"; got != want {
		t.Fatalf("stdout = %q, want %q", got, want)
	}
}

func TestSessionsListCommand_ListErrorPropagates(t *testing.T) {
	wantErr := errors.New("sessiondb: boom")
	fake := &sessionsFakeStore{listErr: wantErr}

	cmd := newSessionsCommandWithOpener(openerFor(fake, "/repo/a"))
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"list"})

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want the store's ListMeta error to propagate")
	}
	if !errors.Is(err, wantErr) {
		t.Fatalf("Execute() error = %v, want it to wrap %v", err, wantErr)
	}
}

func TestSessionsRmCommand_DeletesAndConfirms(t *testing.T) {
	fake := &sessionsFakeStore{}

	cmd := newSessionsCommandWithOpener(openerFor(fake, "/repo/a"))
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"rm", "s1"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil", err)
	}
	if fake.deletedID != "s1" {
		t.Fatalf("deletedID = %q, want %q", fake.deletedID, "s1")
	}
	if !strings.Contains(out.String(), "s1") {
		t.Fatalf("stdout = %q, want a confirmation mentioning the deleted id", out.String())
	}
}

func TestSessionsRmCommand_MissingIDIsSuccessNoOp(t *testing.T) {
	fake := &sessionsFakeStore{}

	cmd := newSessionsCommandWithOpener(openerFor(fake, "/repo/a"))
	out := new(bytes.Buffer)
	cmd.SetOut(out)
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"rm", "does-not-exist"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("Execute() error = %v, want nil for the no-op delete contract", err)
	}
	if fake.deletedID != "does-not-exist" {
		t.Fatalf("deletedID = %q, want %q", fake.deletedID, "does-not-exist")
	}
}

func TestSessionsRmCommand_OpenerErrorPropagates(t *testing.T) {
	wantErr := errors.New("sessions: load config: boom")
	opener := func() (sessionStore, string, error) { return nil, "", wantErr }

	cmd := newSessionsCommandWithOpener(opener)
	cmd.SetOut(new(bytes.Buffer))
	cmd.SetErr(new(bytes.Buffer))
	cmd.SetArgs([]string{"rm", "s1"})

	err := cmd.Execute()
	if err == nil {
		t.Fatal("Execute() error = nil, want the opener error to propagate")
	}
	if !errors.Is(err, wantErr) {
		t.Fatalf("Execute() error = %v, want it to wrap %v", err, wantErr)
	}
}
