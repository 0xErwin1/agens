package auth

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestLoadHappyPathAndAPIKey(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")
	if err := os.WriteFile(path, []byte(`{"openai-api":{"api_key":"sk-x"}}`), 0o600); err != nil {
		t.Fatal(err)
	}

	file, err := Load(path)
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}

	key, err := file.APIKey("openai-api")
	if err != nil {
		t.Fatalf("APIKey() error = %v", err)
	}
	if key != "sk-x" {
		t.Fatalf("APIKey() = %q, want %q", key, "sk-x")
	}
}

func TestLoadMissingFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "does-not-exist.json")

	_, err := Load(path)
	if err == nil {
		t.Fatal("Load() error = nil, want error")
	}
	if !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("Load() error = %v, want errors.Is(err, os.ErrNotExist)", err)
	}
	if !strings.Contains(err.Error(), path) {
		t.Fatalf("Load() error = %v, want it to name path %q", err, path)
	}
}

func TestLoadMalformedJSON(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")
	if err := os.WriteFile(path, []byte(`{not valid json`), 0o600); err != nil {
		t.Fatal(err)
	}

	_, err := Load(path)
	if err == nil {
		t.Fatal("Load() error = nil, want error")
	}
	var syntaxErr *json.SyntaxError
	if !errors.As(err, &syntaxErr) {
		t.Fatalf("Load() error = %v, want wrapped json.SyntaxError", err)
	}
}

func TestAPIKeyMissingProviderOrEmptyKey(t *testing.T) {
	tests := []struct {
		name       string
		file       File
		providerID string
	}{
		{
			name:       "provider absent",
			file:       File{},
			providerID: "openai-api",
		},
		{
			name:       "api_key empty",
			file:       File{"openai-api": Entry{APIKey: ""}},
			providerID: "openai-api",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, err := tt.file.APIKey(tt.providerID)
			if err == nil {
				t.Fatal("APIKey() error = nil, want error")
			}
			if !strings.Contains(err.Error(), tt.providerID) {
				t.Fatalf("APIKey() error = %v, want it to name provider %q", err, tt.providerID)
			}
		})
	}
}

func TestErrorsNeverLeakAPIKeyValue(t *testing.T) {
	const secret = "sk-super-secret-value"

	dir := t.TempDir()
	path := filepath.Join(dir, "auth.json")
	if err := os.WriteFile(path, []byte(`{"openai-api":{"api_key":"`+secret+`"}}`), 0o600); err != nil {
		t.Fatal(err)
	}

	file, err := Load(path)
	if err != nil {
		t.Fatalf("Load() error = %v", err)
	}

	if _, err := file.APIKey("anthropic-api"); err != nil {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("APIKey() error leaked secret: %v", err)
		}
	}

	missingPath := filepath.Join(dir, "missing.json")
	if _, err := Load(missingPath); err != nil {
		if strings.Contains(err.Error(), secret) {
			t.Fatalf("Load() error leaked secret: %v", err)
		}
	}
}
