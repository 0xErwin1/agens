package fs

import (
	"encoding/json"
	"io/fs"
	"os"
	"path/filepath"
	"testing"

	"github.com/google/jsonschema-go/jsonschema"
)

// TestSchemaShape confirms the jsonschema-go API shape this package's tools
// rely on: an object schema built from Type, Properties (map[string]*Schema)
// and Required ([]string) marshals to the expected JSON Schema wire form.
func TestSchemaShape(t *testing.T) {
	schema := &jsonschema.Schema{
		Type: "object",
		Properties: map[string]*jsonschema.Schema{
			"path": {Type: "string", Description: "file path"},
		},
		Required: []string{"path"},
	}

	data, err := json.Marshal(schema)
	if err != nil {
		t.Fatalf("json.Marshal(schema) error = %v", err)
	}

	var decoded map[string]any
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal(data) error = %v", err)
	}

	if decoded["type"] != "object" {
		t.Fatalf("decoded[%q] = %v, want %q", "type", decoded["type"], "object")
	}

	props, ok := decoded["properties"].(map[string]any)
	if !ok {
		t.Fatalf("decoded[%q] = %T, want map[string]any", "properties", decoded["properties"])
	}
	if _, ok := props["path"]; !ok {
		t.Fatalf("decoded[%q] missing key %q", "properties", "path")
	}

	required, ok := decoded["required"].([]any)
	if !ok || len(required) != 1 || required[0] != "path" {
		t.Fatalf("decoded[%q] = %v, want [%q]", "required", decoded["required"], "path")
	}
}

func TestOpen(t *testing.T) {
	t.Run("valid directory succeeds", func(t *testing.T) {
		root := t.TempDir()

		d, err := Open(root)
		if err != nil {
			t.Fatalf("Open(%q) error = %v", root, err)
		}
		if d == nil {
			t.Fatalf("Open(%q) = nil Dir, want non-nil", root)
		}
	})

	t.Run("non-existent directory fails", func(t *testing.T) {
		missing := filepath.Join(t.TempDir(), "does-not-exist")

		_, err := Open(missing)
		if err == nil {
			t.Fatalf("Open(%q) error = nil, want error", missing)
		}
	})
}

// TestDirRel_ReadEscapes exercises the confinement boundary against read-style
// access (root.ReadFile via d.rel), asserting escapes are rejected and the
// external target is left untouched.
func TestDirRel_ReadEscapes(t *testing.T) {
	root := t.TempDir()
	outsideDir := t.TempDir()

	outsideFile := filepath.Join(outsideDir, "outside.txt")
	writeFileT(t, outsideFile, "external content")

	symlink := filepath.Join(root, "link-to-outside")
	if err := os.Symlink(outsideFile, symlink); err != nil {
		t.Fatalf("os.Symlink error = %v", err)
	}

	d, err := Open(root)
	if err != nil {
		t.Fatalf("Open(%q) error = %v", root, err)
	}

	tests := []struct {
		name string
		path string
	}{
		{name: "parent traversal", path: filepath.Join("..", filepath.Base(outsideDir), "outside.txt")},
		{name: "absolute path outside root", path: outsideFile},
		{name: "symlink inside pointing outside", path: "link-to-outside"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			rel, relErr := d.rel(tt.path)
			if relErr == nil {
				if _, err := d.root.ReadFile(rel); err == nil {
					t.Fatalf("ReadFile(%q) error = nil, want an error (escape must be rejected)", rel)
				}
			}

			got := readFileT(t, outsideFile)
			if got != "external content" {
				t.Fatalf("external file content = %q, want %q (must remain untouched)", got, "external content")
			}
		})
	}
}

// TestDirRel_WriteEscapes exercises the confinement boundary against
// write-style access (root.MkdirAll + root.WriteFile via d.rel).
func TestDirRel_WriteEscapes(t *testing.T) {
	root := t.TempDir()
	outsideDir := t.TempDir()

	t.Run("new nested path inside root is allowed", func(t *testing.T) {
		d, err := Open(root)
		if err != nil {
			t.Fatalf("Open(%q) error = %v", root, err)
		}

		rel, err := d.rel(filepath.Join("a", "b", "c.txt"))
		if err != nil {
			t.Fatalf("rel error = %v", err)
		}
		if err := d.root.MkdirAll(filepath.Dir(rel), 0o755); err != nil {
			t.Fatalf("MkdirAll(%q) error = %v", filepath.Dir(rel), err)
		}
		if err := d.root.WriteFile(rel, []byte("nested"), 0o644); err != nil {
			t.Fatalf("WriteFile(%q) error = %v", rel, err)
		}

		got := readFileT(t, filepath.Join(root, "a", "b", "c.txt"))
		if got != "nested" {
			t.Fatalf("nested file content = %q, want %q", got, "nested")
		}
	})

	t.Run("parent traversal on write is rejected", func(t *testing.T) {
		d, err := Open(root)
		if err != nil {
			t.Fatalf("Open(%q) error = %v", root, err)
		}

		escapePath := filepath.Join("..", filepath.Base(outsideDir), "created.txt")
		rel, relErr := d.rel(escapePath)
		if relErr == nil {
			if err := d.root.MkdirAll(filepath.Dir(rel), 0o755); err == nil {
				if err := d.root.WriteFile(rel, []byte("should not land"), 0o644); err == nil {
					t.Fatalf("WriteFile(%q) succeeded, want an error (escape must be rejected)", rel)
				}
			}
		}

		if _, err := os.Stat(filepath.Join(outsideDir, "created.txt")); !os.IsNotExist(err) {
			t.Fatalf("os.Stat(created.txt outside root) error = %v, want IsNotExist (must remain untouched)", err)
		}
	})

	t.Run("write through symlinked parent pointing outside is rejected", func(t *testing.T) {
		linkParent := filepath.Join(root, "link-parent")
		if err := os.Symlink(outsideDir, linkParent); err != nil {
			t.Fatalf("os.Symlink error = %v", err)
		}

		d, err := Open(root)
		if err != nil {
			t.Fatalf("Open(%q) error = %v", root, err)
		}

		rel, relErr := d.rel(filepath.Join("link-parent", "escaped.txt"))
		if relErr == nil {
			if err := d.root.WriteFile(rel, []byte("should not land"), 0o644); err == nil {
				t.Fatalf("WriteFile(%q) succeeded, want an error (escape via symlinked parent must be rejected)", rel)
			}
		}

		if _, err := os.Stat(filepath.Join(outsideDir, "escaped.txt")); !os.IsNotExist(err) {
			t.Fatalf("os.Stat(escaped.txt outside root) error = %v, want IsNotExist (must remain untouched)", err)
		}
	})
}

// TestDirFS confirms FS() returns an fs.FS confined identically to d.rel:
// reads inside the root succeed, and an escaping symlink is rejected.
func TestDirFS(t *testing.T) {
	root := t.TempDir()
	outsideDir := t.TempDir()

	writeFileT(t, filepath.Join(root, "inside.txt"), "inside content")

	outsideFile := filepath.Join(outsideDir, "outside.txt")
	writeFileT(t, outsideFile, "external content")

	symlink := filepath.Join(root, "link-to-outside")
	if err := os.Symlink(outsideFile, symlink); err != nil {
		t.Fatalf("os.Symlink error = %v", err)
	}

	d, err := Open(root)
	if err != nil {
		t.Fatalf("Open(%q) error = %v", root, err)
	}

	fsys := d.FS()
	if fsys == nil {
		t.Fatalf("FS() = nil, want a usable fs.FS")
	}

	got, err := fs.ReadFile(fsys, "inside.txt")
	if err != nil {
		t.Fatalf("fs.ReadFile(inside.txt) error = %v", err)
	}
	if string(got) != "inside content" {
		t.Fatalf("fs.ReadFile(inside.txt) = %q, want %q", got, "inside content")
	}

	if _, err := fs.ReadFile(fsys, "link-to-outside"); err == nil {
		t.Fatalf("fs.ReadFile(link-to-outside) error = nil, want an error (escape must be rejected)")
	}
}

func writeFileT(t *testing.T, path, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("os.WriteFile(%q) error = %v", path, err)
	}
}

func readFileT(t *testing.T, path string) string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("os.ReadFile(%q) error = %v", path, err)
	}
	return string(data)
}
