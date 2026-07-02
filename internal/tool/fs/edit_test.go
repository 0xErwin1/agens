package fs

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

func TestEdit_Execute(t *testing.T) {
	root := t.TempDir()
	outsideDir := t.TempDir()

	outsideFile := filepath.Join(outsideDir, "outside.txt")
	writeFileT(t, outsideFile, "external content")

	d, err := Open(root)
	if err != nil {
		t.Fatalf("Open(%q) error = %v", root, err)
	}
	e := NewEdit(d)

	tests := []struct {
		name        string
		setup       func(t *testing.T) string
		input       func(path string) string
		wantErr     bool
		wantContent string
		wantDiff    bool
	}{
		{
			name: "single match replaced with diff",
			setup: func(t *testing.T) string {
				path := filepath.Join(root, "single.txt")
				writeFileT(t, path, "before\ntarget\nafter\n")
				return "single.txt"
			},
			input: func(path string) string {
				return `{"path":"` + path + `","old_string":"target","new_string":"replaced"}`
			},
			wantContent: "before\nreplaced\nafter\n",
			wantDiff:    true,
		},
		{
			name: "zero matches",
			setup: func(t *testing.T) string {
				path := filepath.Join(root, "zero.txt")
				writeFileT(t, path, "nothing here\n")
				return "zero.txt"
			},
			input: func(path string) string {
				return `{"path":"` + path + `","old_string":"missing","new_string":"x"}`
			},
			wantErr:     true,
			wantContent: "nothing here\n",
		},
		{
			name: "multiple matches",
			setup: func(t *testing.T) string {
				path := filepath.Join(root, "multi.txt")
				writeFileT(t, path, "dup\ndup\n")
				return "multi.txt"
			},
			input: func(path string) string {
				return `{"path":"` + path + `","old_string":"dup","new_string":"x"}`
			},
			wantErr:     true,
			wantContent: "dup\ndup\n",
		},
		{
			name: "old equals new is a no-op rejected",
			setup: func(t *testing.T) string {
				path := filepath.Join(root, "noop.txt")
				writeFileT(t, path, "same\n")
				return "noop.txt"
			},
			input: func(path string) string {
				return `{"path":"` + path + `","old_string":"same\n","new_string":"same\n"}`
			},
			wantErr:     true,
			wantContent: "same\n",
		},
		{
			name: "path escape",
			setup: func(t *testing.T) string {
				return filepath.Join("..", filepath.Base(outsideDir), "outside.txt")
			},
			input: func(path string) string {
				return `{"path":"` + path + `","old_string":"external","new_string":"x"}`
			},
			wantErr: true,
		},
		{
			name: "read on directory",
			setup: func(t *testing.T) string {
				if err := os.Mkdir(filepath.Join(root, "adir"), 0o755); err != nil {
					t.Fatalf("os.Mkdir error = %v", err)
				}
				return "adir"
			},
			input: func(path string) string {
				return `{"path":"` + path + `","old_string":"a","new_string":"b"}`
			},
			wantErr: true,
		},
		{
			name: "invalid JSON input",
			setup: func(t *testing.T) string {
				return "whatever.txt"
			},
			input: func(path string) string {
				return `{"path":`
			},
			wantErr: true,
		},
		{
			name: "empty path",
			setup: func(t *testing.T) string {
				return ""
			},
			input: func(path string) string {
				return `{"path":"","old_string":"a","new_string":"b"}`
			},
			wantErr: true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			relPath := tt.setup(t)

			res, err := e.Execute(context.Background(), json.RawMessage(tt.input(relPath)))
			if err != nil {
				t.Fatalf("Execute() error = %v, want nil (domain failures must be IsError, not a Go error)", err)
			}
			if res.IsError != tt.wantErr {
				t.Fatalf("Execute() IsError = %v, want %v (Text = %q)", res.IsError, tt.wantErr, res.Text)
			}
			if tt.wantDiff {
				want := "--- a/" + relPath + "\n"
				if len(res.Text) < len(want) || res.Text[:len(want)] != want {
					t.Fatalf("Execute() Text = %q, want it to start with %q", res.Text, want)
				}
			}
			if tt.wantContent != "" {
				got := readFileT(t, filepath.Join(root, relPath))
				if got != tt.wantContent {
					t.Fatalf("file %q content = %q, want %q", relPath, got, tt.wantContent)
				}
			}

			gotOutside := readFileT(t, outsideFile)
			if gotOutside != "external content" {
				t.Fatalf("external file content = %q, want %q (must remain untouched)", gotOutside, "external content")
			}
		})
	}
}

func TestEdit_Name(t *testing.T) {
	e := NewEdit(mustOpenT(t, t.TempDir()))
	if got := e.Name(); got != "edit" {
		t.Fatalf("Name() = %q, want %q", got, "edit")
	}
}

func TestEdit_Schema(t *testing.T) {
	e := NewEdit(mustOpenT(t, t.TempDir()))

	schema := e.Schema()
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
	if !ok || len(required) != 3 {
		t.Fatalf("Schema() required = %v, want 3 fields", decoded["required"])
	}
}

func TestNewEdit_PanicsOnNilDir(t *testing.T) {
	defer func() {
		if recover() == nil {
			t.Fatalf("NewEdit(nil) did not panic, want a panic")
		}
	}()
	NewEdit(nil)
}
