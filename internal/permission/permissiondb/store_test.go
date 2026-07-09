package permissiondb

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"testing"

	"github.com/0xErwin1/agens/internal/permission"
)

func TestStore_GrantSurvivesRestartScopedToProjectAndArgument(t *testing.T) {
	path := filepath.Join(t.TempDir(), "permissions.db")
	ctx := context.Background()

	store := openTestStore(t, path, "/home/me/projA")
	grant := permission.Rule{Decision: permission.DecisionAllow, Name: "bash", Argument: "git status"}
	if err := store.Append(ctx, grant); err != nil {
		t.Fatalf("Append() error = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatalf("Close() error = %v", err)
	}

	reopened := openTestStore(t, path, "/home/me/projA")
	rules, err := reopened.Rules(ctx)
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	if len(rules) != 1 || rules[0] != grant {
		t.Fatalf("Rules() after restart = %+v, want [%+v]", rules, grant)
	}

	differentArgument := permission.Rule{Decision: permission.DecisionAllow, Name: "bash", Argument: "git push"}
	if containsRule(rules, differentArgument) {
		t.Fatalf("Rules() = %+v, must not contain a grant for a different argument", rules)
	}

	otherProject := openTestStore(t, path, "/home/me/projB")
	crossProjectRules, err := otherProject.Rules(ctx)
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	if len(crossProjectRules) != 0 {
		t.Fatalf("Rules() for a different project = %+v, want none (cross-project isolation)", crossProjectRules)
	}
}

func TestStore_ConcurrentAppendsAreSerialized(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "permissions.db"), "/home/me/proj")

	const total = 20
	var wg sync.WaitGroup
	errs := make(chan error, total)
	for i := 0; i < total; i++ {
		i := i
		wg.Add(1)
		go func() {
			defer wg.Done()
			errs <- store.Append(context.Background(), permission.Rule{
				Decision: permission.DecisionAllow,
				Name:     "bash",
				Argument: fmt.Sprintf("cmd-%d", i),
			})
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatalf("Append() error = %v", err)
		}
	}

	rules, err := store.Rules(context.Background())
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	if len(rules) != total {
		t.Fatalf("len(Rules()) = %d, want %d", len(rules), total)
	}
}

func TestStore_FreshDatabaseIsCreatedLazilyAndHasNoGrants(t *testing.T) {
	path := filepath.Join(t.TempDir(), "nested", "permissions.db")
	store := openTestStore(t, path, "/home/me/proj")

	if _, err := os.Stat(path); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("database exists before first access or stat failed: %v", err)
	}

	rules, err := store.Rules(context.Background())
	if err != nil {
		t.Fatalf("Rules() on a fresh store error = %v", err)
	}
	if len(rules) != 0 {
		t.Fatalf("Rules() on a fresh store = %+v, want none", rules)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("database was not created at %q: %v", path, err)
	}
}

func TestStore_AppendIsIdempotentForTheSameGrant(t *testing.T) {
	store := openTestStore(t, filepath.Join(t.TempDir(), "permissions.db"), "/home/me/proj")
	ctx := context.Background()

	grant := permission.Rule{Decision: permission.DecisionAllow, Name: "bash", Argument: "git status"}
	if err := store.Append(ctx, grant); err != nil {
		t.Fatalf("Append() error = %v", err)
	}
	if err := store.Append(ctx, grant); err != nil {
		t.Fatalf("Append() (repeat) error = %v", err)
	}

	rules, err := store.Rules(ctx)
	if err != nil {
		t.Fatalf("Rules() error = %v", err)
	}
	if len(rules) != 1 {
		t.Fatalf("Rules() = %+v, want a single deduplicated grant", rules)
	}
}

func TestDefaultPathUsesXDGDataHome(t *testing.T) {
	dataHome := t.TempDir()
	t.Setenv("XDG_DATA_HOME", dataHome)

	want := filepath.Join(dataHome, "agens", "permissions.db")
	if got := DefaultPath(); got != want {
		t.Fatalf("DefaultPath() = %q, want %q", got, want)
	}
}

func openTestStore(t *testing.T, path, project string) *Store {
	t.Helper()
	store, err := Open(path, project)
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

func containsRule(rules []permission.Rule, want permission.Rule) bool {
	for _, r := range rules {
		if r == want {
			return true
		}
	}
	return false
}
