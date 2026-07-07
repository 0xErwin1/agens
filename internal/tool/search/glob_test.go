package search

import (
	"context"
	"encoding/json"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/0xErwin1/agens/internal/tool"
)

// openFS opens root as an os.Root and returns the confined fs.FS backing it,
// mirroring the boundary internal/tool/fs.Dir.FS() exposes in production.
func openFS(t *testing.T, root string) fs.FS {
	t.Helper()

	r, err := os.OpenRoot(root)
	if err != nil {
		t.Fatalf("os.OpenRoot(%q) error = %v", root, err)
	}
	return r.FS()
}

func writeGlobFile(t *testing.T, root, rel, content string) {
	t.Helper()

	full := filepath.Join(root, rel)
	if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
		t.Fatalf("os.MkdirAll(%q) error = %v", filepath.Dir(full), err)
	}
	if err := os.WriteFile(full, []byte(content), 0o644); err != nil {
		t.Fatalf("os.WriteFile(%q) error = %v", full, err)
	}
}

func mustGlobResult(t *testing.T, g *Glob, pattern string) tool.Result {
	t.Helper()

	input, err := json.Marshal(map[string]string{"pattern": pattern})
	if err != nil {
		t.Fatalf("json.Marshal error = %v", err)
	}

	res, err := g.Execute(context.Background(), input)
	if err != nil {
		t.Fatalf("Execute(%q) error = %v, want nil", pattern, err)
	}
	return res
}

func TestGlob_Basic(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.go", "package a")
	writeGlobFile(t, root, "b.txt", "text")

	g := NewGlob(openFS(t, root))

	res := mustGlobResult(t, g, "*.go")
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if res.Text != "a.go" {
		t.Fatalf("Text = %q, want %q", res.Text, "a.go")
	}
}

func TestGlob_Recursive(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "x/y.go", "package y")
	writeGlobFile(t, root, "top.go", "package top")
	writeGlobFile(t, root, "x/y.txt", "text")

	g := NewGlob(openFS(t, root))

	res := mustGlobResult(t, g, "**/*.go")
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}

	got := strings.Split(res.Text, "\n")
	want := map[string]bool{"x/y.go": true, "top.go": true}
	if len(got) != len(want) {
		t.Fatalf("Text = %q, want exactly %d entries", res.Text, len(want))
	}
	for _, p := range got {
		if !want[p] {
			t.Fatalf("unexpected path %q in Text = %q", p, res.Text)
		}
	}
}

func TestGlob_FilesOnly(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "sub/inner.go", "package inner")
	if err := os.Mkdir(filepath.Join(root, "sub.go"), 0o755); err != nil {
		t.Fatalf("os.Mkdir error = %v", err)
	}

	g := NewGlob(openFS(t, root))

	res := mustGlobResult(t, g, "*.go")
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if res.Text != "no matches" {
		t.Fatalf("Text = %q, want %q (the directory sub.go must not be listed)", res.Text, "no matches")
	}
}

func TestGlob_NoMatch(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.txt", "text")

	g := NewGlob(openFS(t, root))

	res := mustGlobResult(t, g, "*.go")
	if res.IsError {
		t.Fatalf("IsError = true, want false")
	}
	if res.Text != "no matches" {
		t.Fatalf("Text = %q, want %q", res.Text, "no matches")
	}
}

func TestGlob_OverCap(t *testing.T) {
	root := t.TempDir()
	for i := 0; i < maxItems+5; i++ {
		writeGlobFile(t, root, fmt.Sprintf("f%04d.go", i), "package f")
	}

	g := NewGlob(openFS(t, root))

	res := mustGlobResult(t, g, "*.go")
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}

	const notice = "\n[output truncated after 1000 paths]"
	if !strings.HasSuffix(res.Text, notice) {
		t.Fatalf("Text does not end with %q; got suffix %q", notice, res.Text[max(0, len(res.Text)-60):])
	}
	content := strings.TrimSuffix(res.Text, notice)
	if got := len(strings.Split(content, "\n")); got != maxItems {
		t.Fatalf("got %d matched paths, want exactly %d", got, maxItems)
	}
}

func TestGlob_HiddenSkipped(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, ".git/config.go", "package config")
	writeGlobFile(t, root, "visible.go", "package visible")

	g := NewGlob(openFS(t, root))

	res := mustGlobResult(t, g, "**/*.go")
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if res.Text != "visible.go" {
		t.Fatalf("Text = %q, want %q (.git contents must be skipped)", res.Text, "visible.go")
	}
}

func TestGlob_BadPattern(t *testing.T) {
	root := t.TempDir()

	g := NewGlob(openFS(t, root))

	res := mustGlobResult(t, g, "a[")
	if !res.IsError {
		t.Fatalf("IsError = false, want true for a malformed pattern")
	}
}

func TestGlob_MalformedJSON(t *testing.T) {
	g := NewGlob(openFS(t, t.TempDir()))

	res, err := g.Execute(context.Background(), []byte("not json"))
	if err != nil {
		t.Fatalf("Execute error = %v, want nil", err)
	}
	if !res.IsError {
		t.Fatalf("IsError = false, want true for malformed JSON")
	}
}

func TestGlob_EmptyPattern(t *testing.T) {
	res := mustGlobResult(t, NewGlob(openFS(t, t.TempDir())), "")
	if !res.IsError {
		t.Fatalf("IsError = false, want true for an empty pattern")
	}
}

func TestGlob_Confinement(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	writeGlobFile(t, outside, "secret.go", "package secret")

	symlink := filepath.Join(root, "escape")
	if err := os.Symlink(outside, symlink); err != nil {
		t.Fatalf("os.Symlink error = %v", err)
	}

	g := NewGlob(openFS(t, root))

	tests := []struct {
		name    string
		pattern string
	}{
		{name: "parent traversal", pattern: "../outside/secret.go"},
		{name: "absolute outside", pattern: filepath.Join(outside, "secret.go")},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			res := mustGlobResult(t, g, tt.pattern)
			if !res.IsError {
				t.Fatalf("IsError = false, want true for an escaping pattern")
			}
			if strings.Contains(res.Text, "package secret") {
				t.Fatalf("Text = %q, must not contain the external file's content", res.Text)
			}
		})
	}

	t.Run("symlink inside pointing outside is not followed", func(t *testing.T) {
		res := mustGlobResult(t, g, "escape/*.go")
		if res.IsError {
			t.Fatalf("IsError = true, want false; Text = %q", res.Text)
		}
		if strings.Contains(res.Text, "secret") {
			t.Fatalf("Text = %q, must not list the external file via the symlink", res.Text)
		}
	})
}
