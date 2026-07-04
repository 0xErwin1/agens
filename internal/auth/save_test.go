package auth

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestSaveWritesAtomicallyInSameDir(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")

	if err := Save(path, File{"openai-api": {APIKey: "sk-x"}}); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("ReadDir() error = %v", err)
	}
	if len(entries) != 1 || entries[0].Name() != "auth.json" {
		t.Fatalf("dir entries = %v, want exactly one entry named auth.json (no leftover temp files)", entries)
	}
}

func TestSaveSetsFileAndDirModes(t *testing.T) {
	base := t.TempDir()
	dir := filepath.Join(base, "nested", "config")
	path := filepath.Join(dir, "auth.json")

	if err := Save(path, File{"openai-api": {APIKey: "sk-x"}}); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	fileInfo, err := os.Stat(path)
	if err != nil {
		t.Fatalf("Stat(file) error = %v", err)
	}
	if got, want := fileInfo.Mode().Perm(), os.FileMode(0o600); got != want {
		t.Fatalf("file mode = %v, want %v", got, want)
	}

	dirInfo, err := os.Stat(dir)
	if err != nil {
		t.Fatalf("Stat(dir) error = %v", err)
	}
	if got, want := dirInfo.Mode().Perm(), os.FileMode(0o700); got != want {
		t.Fatalf("dir mode = %v, want %v", got, want)
	}
}

func TestSaveRoundTripsFullFileViaLoad(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")

	expiresAt := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	want := File{
		"openai-api": {
			APIKey:    "sk-openai",
			AccountID: "acct-1",
		},
		"anthropic-api": {
			AccessToken:  "at-anthropic",
			RefreshToken: "rt-anthropic",
			ExpiresAt:    &expiresAt,
		},
	}

	if err := Save(path, want); err != nil {
		t.Fatalf("Save() error = %v", err)
	}

	got, err := Load(path)
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}

	if len(got) != len(want) {
		t.Fatalf("Load() returned %d providers, want %d", len(got), len(want))
	}
	for id, wantEntry := range want {
		gotEntry, ok := got[id]
		if !ok {
			t.Fatalf("Load() missing provider %q", id)
		}
		if gotEntry.APIKey != wantEntry.APIKey ||
			gotEntry.AccessToken != wantEntry.AccessToken ||
			gotEntry.RefreshToken != wantEntry.RefreshToken ||
			gotEntry.AccountID != wantEntry.AccountID {
			t.Fatalf("Load()[%q] = %+v, want %+v", id, gotEntry, wantEntry)
		}
		switch {
		case wantEntry.ExpiresAt == nil && gotEntry.ExpiresAt != nil:
			t.Fatalf("Load()[%q].ExpiresAt = %v, want nil", id, gotEntry.ExpiresAt)
		case wantEntry.ExpiresAt != nil && gotEntry.ExpiresAt == nil:
			t.Fatalf("Load()[%q].ExpiresAt = nil, want %v", id, wantEntry.ExpiresAt)
		case wantEntry.ExpiresAt != nil && !gotEntry.ExpiresAt.Equal(*wantEntry.ExpiresAt):
			t.Fatalf("Load()[%q].ExpiresAt = %v, want %v", id, gotEntry.ExpiresAt, wantEntry.ExpiresAt)
		}
	}
}

func TestSaveFailurePreservesExistingFileAndLeavesNoTempFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")

	existing := []byte(`{"openai-api":{"api_key":"sk-existing"}}`)
	if err := os.WriteFile(path, existing, 0o600); err != nil {
		t.Fatal(err)
	}

	if err := os.Chmod(dir, 0o500); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = os.Chmod(dir, 0o700)
	})

	err := Save(path, File{"anthropic-api": {APIKey: "sk-new"}})
	if err == nil {
		t.Fatal("Save() error = nil, want an error when the parent directory is not writable")
	}

	if err := os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}

	got, readErr := os.ReadFile(path)
	if readErr != nil {
		t.Fatalf("ReadFile(path) error = %v", readErr)
	}
	if string(got) != string(existing) {
		t.Fatalf("existing auth.json content = %q, want unchanged %q", got, existing)
	}

	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatalf("ReadDir() error = %v", err)
	}
	for _, entry := range entries {
		if entry.Name() != "auth.json" {
			t.Fatalf("leftover file %q found after failed Save(), want only auth.json", entry.Name())
		}
	}
}

func TestSaveErrorNeverLeaksSecretValue(t *testing.T) {
	const secret = "sk-super-secret-save-value"

	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")
	if err := os.Chmod(dir, 0o500); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = os.Chmod(dir, 0o700)
	})

	err := Save(path, File{"openai-api": {
		APIKey:       secret,
		AccessToken:  secret,
		RefreshToken: secret,
	}})
	if err == nil {
		t.Fatal("Save() error = nil, want an error when the parent directory is not writable")
	}
	if strings.Contains(err.Error(), secret) {
		t.Fatalf("Save() error leaked secret: %v", err)
	}
}
