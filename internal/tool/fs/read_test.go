package fs

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRead_Execute(t *testing.T) {
	root := t.TempDir()
	outsideDir := t.TempDir()

	outsideFile := filepath.Join(outsideDir, "outside.txt")
	writeFileT(t, outsideFile, "external content")

	writeFileT(t, filepath.Join(root, "whole.txt"), "one\ntwo\nthree\n")
	writeFileT(t, filepath.Join(root, "lines.txt"), "line1\nline2\nline3\nline4\nline5\n")
	if err := os.Mkdir(filepath.Join(root, "subdir"), 0o755); err != nil {
		t.Fatalf("os.Mkdir error = %v", err)
	}
	symlink := filepath.Join(root, "link-to-outside")
	if err := os.Symlink(outsideFile, symlink); err != nil {
		t.Fatalf("os.Symlink error = %v", err)
	}

	d, err := Open(root)
	if err != nil {
		t.Fatalf("Open(%q) error = %v", root, err)
	}
	r := NewRead(d)

	tests := []struct {
		name       string
		input      string
		wantErr    bool
		wantText   string
		wantSubstr string
	}{
		{
			name:     "whole file read",
			input:    `{"path":"whole.txt"}`,
			wantText: "one\ntwo\nthree\n",
		},
		{
			name:     "offset and limit line slicing",
			input:    `{"path":"lines.txt","offset":2,"limit":2}`,
			wantText: "line2\nline3",
		},
		{
			name:    "offset past EOF",
			input:   `{"path":"lines.txt","offset":100}`,
			wantErr: true,
		},
		{
			name:    "missing file",
			input:   `{"path":"does-not-exist.txt"}`,
			wantErr: true,
		},
		{
			name:    "directory path",
			input:   `{"path":"subdir"}`,
			wantErr: true,
		},
		{
			name:    "invalid JSON input",
			input:   `{"path":`,
			wantErr: true,
		},
		{
			name:    "empty path",
			input:   `{"path":""}`,
			wantErr: true,
		},
		{
			name:    "parent traversal escape",
			input:   `{"path":"../` + filepath.Base(outsideDir) + `/outside.txt"}`,
			wantErr: true,
		},
		{
			name:    "absolute path outside root",
			input:   `{"path":"` + strings.ReplaceAll(outsideFile, `\`, `\\`) + `"}`,
			wantErr: true,
		},
		{
			name:    "symlink inside pointing outside",
			input:   `{"path":"link-to-outside"}`,
			wantErr: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			res, err := r.Execute(context.Background(), json.RawMessage(tt.input))
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil (domain failures must be IsError, not a Go error)", err)
			}
			if res.IsError != tt.wantErr {
				t.Fatalf("Execute(%s) IsError = %v, want %v (Text = %q)", tt.input, res.IsError, tt.wantErr, res.Text)
			}
			if !tt.wantErr && tt.wantText != "" && res.Text != tt.wantText {
				t.Fatalf("Execute(%s) Text = %q, want %q", tt.input, res.Text, tt.wantText)
			}

			got := readFileT(t, outsideFile)
			if got != "external content" {
				t.Fatalf("external file content = %q, want %q (must remain untouched)", got, "external content")
			}
		})
	}
}

func TestRead_Name(t *testing.T) {
	r := NewRead(mustOpenT(t, t.TempDir()))
	if got := r.Name(); got != "read" {
		t.Fatalf("Name() = %q, want %q", got, "read")
	}
}

func TestRead_Schema(t *testing.T) {
	r := NewRead(mustOpenT(t, t.TempDir()))

	schema := r.Schema()
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
	if !ok || len(required) != 1 || required[0] != "path" {
		t.Fatalf("Schema() required = %v, want [%q]", decoded["required"], "path")
	}
}

func TestNewRead_PanicsOnNilDir(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatalf("NewRead(nil) did not panic, want a panic")
		}
	}()
	NewRead(nil)
}

func mustOpenT(t *testing.T, dir string) *Dir {
	t.Helper()
	d, err := Open(dir)
	if err != nil {
		t.Fatalf("Open(%q) error = %v", dir, err)
	}
	return d
}
