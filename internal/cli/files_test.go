package cli

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func TestProjectFileSource_ListSkipsNoiseAndReads(t *testing.T) {
	root := t.TempDir()

	writeFile(t, root, "main.go", "package main")
	writeFile(t, root, "internal/app/app.go", "package app")
	writeFile(t, root, ".git/config", "[core]")
	writeFile(t, root, "node_modules/dep/index.js", "module.exports = {}")

	src, err := newProjectFileSource(root)
	if err != nil {
		t.Fatalf("newProjectFileSource() error = %v", err)
	}

	files, err := src.List()
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}

	if !slices.Contains(files, "main.go") || !slices.Contains(files, filepath.ToSlash("internal/app/app.go")) {
		t.Fatalf("List() = %v, want the project files", files)
	}
	for _, f := range files {
		if strings.HasPrefix(f, ".git/") || strings.HasPrefix(f, "node_modules/") {
			t.Fatalf("List() included a skipped directory: %q", f)
		}
	}

	content, err := src.Read("main.go")
	if err != nil || content != "package main" {
		t.Fatalf("Read(main.go) = (%q, %v), want the file body", content, err)
	}
}

func writeFile(t *testing.T, root, rel, body string) {
	t.Helper()
	path := filepath.Join(root, filepath.FromSlash(rel))
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
}
