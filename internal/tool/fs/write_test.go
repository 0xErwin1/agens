package fs

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestWrite_Execute(t *testing.T) {
	root := t.TempDir()
	outsideDir := t.TempDir()

	outsideFile := filepath.Join(outsideDir, "outside.txt")
	writeFileT(t, outsideFile, "external content")

	writeFileT(t, filepath.Join(root, "existing.txt"), "old content")

	d, err := Open(root)
	if err != nil {
		t.Fatalf("Open(%q) error = %v", root, err)
	}
	w := NewWrite(d)

	tests := []struct {
		name        string
		input       string
		wantErr     bool
		checkPath   string
		wantContent string
	}{
		{
			name:        "create with missing nested parents",
			input:       `{"path":"a/b/c.txt","content":"nested content"}`,
			checkPath:   filepath.Join(root, "a", "b", "c.txt"),
			wantContent: "nested content",
		},
		{
			name:        "overwrite existing file",
			input:       `{"path":"existing.txt","content":"new content"}`,
			checkPath:   filepath.Join(root, "existing.txt"),
			wantContent: "new content",
		},
		{
			name:    "path escape",
			input:   `{"path":"../` + filepath.Base(outsideDir) + `/escaped.txt","content":"should not land"}`,
			wantErr: true,
		},
		{
			name:    "parent dir escape",
			input:   `{"path":"../` + filepath.Base(outsideDir) + `/nested/escaped.txt","content":"should not land"}`,
			wantErr: true,
		},
		{
			name:    "invalid JSON",
			input:   `{"path":`,
			wantErr: true,
		},
		{
			name:    "empty path",
			input:   `{"path":"","content":"x"}`,
			wantErr: true,
		},
		{
			name:    "missing content field",
			input:   `{"path":"nocontent.txt"}`,
			wantErr: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			res, err := w.Execute(context.Background(), json.RawMessage(tt.input))
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil (domain failures must be IsError, not a Go error)", err)
			}
			if res.IsError != tt.wantErr {
				t.Fatalf("Execute(%s) IsError = %v, want %v (Text = %q)", tt.input, res.IsError, tt.wantErr, res.Text)
			}
			if !tt.wantErr && tt.checkPath != "" {
				got := readFileT(t, tt.checkPath)
				if got != tt.wantContent {
					t.Fatalf("file %q content = %q, want %q", tt.checkPath, got, tt.wantContent)
				}
			}

			got := readFileT(t, outsideFile)
			if got != "external content" {
				t.Fatalf("external file content = %q, want %q (must remain untouched)", got, "external content")
			}
		})
	}

	t.Run("missing content creates no file", func(t *testing.T) {
		if _, err := os.Stat(filepath.Join(root, "nocontent.txt")); !os.IsNotExist(err) {
			t.Fatalf("os.Stat(nocontent.txt) error = %v, want IsNotExist (a missing content field must not create/truncate a file)", err)
		}
	})

	t.Run("escape leaves nothing created outside root", func(t *testing.T) {
		if _, err := os.Stat(filepath.Join(outsideDir, "escaped.txt")); !os.IsNotExist(err) {
			t.Fatalf("os.Stat(escaped.txt outside root) error = %v, want IsNotExist (must remain untouched)", err)
		}
		if _, err := os.Stat(filepath.Join(outsideDir, "nested")); !os.IsNotExist(err) {
			t.Fatalf("os.Stat(nested dir outside root) error = %v, want IsNotExist (must remain untouched)", err)
		}
	})
}

func TestWrite_Name(t *testing.T) {
	w := NewWrite(mustOpenT(t, t.TempDir()))
	if got := w.Name(); got != "write" {
		t.Fatalf("Name() = %q, want %q", got, "write")
	}
}

func TestWrite_Schema(t *testing.T) {
	w := NewWrite(mustOpenT(t, t.TempDir()))

	schema := w.Schema()
	if schema == nil {
		t.Fatalf("Schema() = nil, want non-nil")
	}
	data, err := json.Marshal(schema)
	if err != nil {
		t.Fatalf("json.Marshal(schema) error = %v", err)
	}

	var decoded map[string]any
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal error = %v", err)
	}
	required, ok := decoded["required"].([]any)
	if !ok || len(required) != 2 {
		t.Fatalf("Schema() required = %v, want 2 fields", decoded["required"])
	}
}

func TestNewWrite_PanicsOnNilDir(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatalf("NewWrite(nil) did not panic, want a panic")
		}
	}()
	NewWrite(nil)
}
