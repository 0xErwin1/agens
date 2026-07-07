package search

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/0xErwin1/agens/internal/tool"
)

func grepExecute(t *testing.T, g *Grep, in map[string]any) tool.Result {
	t.Helper()

	input, err := json.Marshal(in)
	if err != nil {
		t.Fatalf("json.Marshal error = %v", err)
	}

	res, err := g.Execute(context.Background(), input)
	if err != nil {
		t.Fatalf("Execute(%v) error = %v, want nil", in, err)
	}
	return res
}

func TestGrep_LiteralMatch(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.txt", "hello needle world\nsecond line")

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "needle"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if res.Text != "a.txt:1:hello needle world" {
		t.Fatalf("Text = %q, want %q", res.Text, "a.txt:1:hello needle world")
	}
}

func TestGrep_RegexMatch(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.txt", "foobar baz\nno match here")

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "foo.*bar"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if res.Text != "a.txt:1:foobar baz" {
		t.Fatalf("Text = %q, want %q", res.Text, "a.txt:1:foobar baz")
	}
}

func TestGrep_CaseInsensitive(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.txt", "Needle here")

	g := NewGrep(openFS(t, root))

	t.Run("true matches", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "needle", "case_insensitive": true})
		if res.IsError || res.Text != "a.txt:1:Needle here" {
			t.Fatalf("IsError=%v Text=%q, want IsError=false Text=%q", res.IsError, res.Text, "a.txt:1:Needle here")
		}
	})

	t.Run("false does not match", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "needle", "case_insensitive": false})
		if res.IsError {
			t.Fatalf("IsError = true, want false; Text = %q", res.Text)
		}
		if res.Text != "no matches" {
			t.Fatalf("Text = %q, want %q", res.Text, "no matches")
		}
	})
}

func TestGrep_PathScoping(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "sub/inside.txt", "target here")
	writeGlobFile(t, root, "outside.txt", "target here too")

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "target", "path": "sub"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if res.Text != "sub/inside.txt:1:target here" {
		t.Fatalf("Text = %q, want %q", res.Text, "sub/inside.txt:1:target here")
	}
}

func TestGrep_PathNotDirectory(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.txt", "content")

	g := NewGrep(openFS(t, root))

	t.Run("nonexistent path", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "content", "path": "missing"})
		if !res.IsError {
			t.Fatalf("IsError = false, want true for a nonexistent path")
		}
	})

	t.Run("path is a file, not a directory", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "content", "path": "a.txt"})
		if !res.IsError {
			t.Fatalf("IsError = false, want true when path names a file")
		}
	})
}

func TestGrep_GlobFilter(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.go", "target in go")
	writeGlobFile(t, root, "b.txt", "target in txt")
	writeGlobFile(t, root, "sub/c.go", "target nested go")

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "target", "glob": "**/*.go"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}

	lines := strings.Split(res.Text, "\n")
	if len(lines) != 2 {
		t.Fatalf("got %d matches, want 2; Text = %q", len(lines), res.Text)
	}
	for _, l := range lines {
		if strings.Contains(l, "b.txt") {
			t.Fatalf("Text = %q, must not include the .txt file", res.Text)
		}
	}
}

func TestGrep_NoMatch(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "a.txt", "nothing interesting")

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "needle"})
	if res.IsError {
		t.Fatalf("IsError = true, want false")
	}
	if res.Text != "no matches" {
		t.Fatalf("Text = %q, want %q", res.Text, "no matches")
	}
}

func TestGrep_InvalidRegex(t *testing.T) {
	g := NewGrep(openFS(t, t.TempDir()))

	res := grepExecute(t, g, map[string]any{"pattern": "("})
	if !res.IsError {
		t.Fatalf("IsError = false, want true for an invalid regex")
	}
}

func TestGrep_MalformedJSON(t *testing.T) {
	g := NewGrep(openFS(t, t.TempDir()))

	res, err := g.Execute(context.Background(), []byte("not json"))
	if err != nil {
		t.Fatalf("Execute error = %v, want nil", err)
	}
	if !res.IsError {
		t.Fatalf("IsError = false, want true for malformed JSON")
	}
}

func TestGrep_EmptyPattern(t *testing.T) {
	res := grepExecute(t, NewGrep(openFS(t, t.TempDir())), map[string]any{"pattern": ""})
	if !res.IsError {
		t.Fatalf("IsError = false, want true for an empty pattern")
	}
}

func TestGrep_BinarySkipped(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, "text.txt", "needle in text")

	binPath := filepath.Join(root, "bin.dat")
	binContent := append([]byte("needle prefix "), 0x00, 0x01, 0x02)
	if err := os.WriteFile(binPath, binContent, 0o644); err != nil {
		t.Fatalf("os.WriteFile(%q) error = %v", binPath, err)
	}

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "needle"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if strings.Contains(res.Text, "bin.dat") {
		t.Fatalf("Text = %q, must not include the binary file", res.Text)
	}
	if res.Text != "text.txt:1:needle in text" {
		t.Fatalf("Text = %q, want %q", res.Text, "text.txt:1:needle in text")
	}
}

func TestGrep_HiddenDirSkipped(t *testing.T) {
	root := t.TempDir()
	writeGlobFile(t, root, ".git/config", "needle in git config")
	writeGlobFile(t, root, "visible.txt", "needle visible")

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "needle"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if strings.Contains(res.Text, ".git") {
		t.Fatalf("Text = %q, must not include .git contents", res.Text)
	}
	if res.Text != "visible.txt:1:needle visible" {
		t.Fatalf("Text = %q, want %q", res.Text, "visible.txt:1:needle visible")
	}

	t.Run("explicitly scoped hidden path is still searched", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "needle", "path": ".git"})
		if res.IsError {
			t.Fatalf("IsError = true, want false; Text = %q", res.Text)
		}
		if res.Text != ".git/config:1:needle in git config" {
			t.Fatalf("Text = %q, want %q", res.Text, ".git/config:1:needle in git config")
		}
	})
}

func TestGrep_LargeFileSkipped(t *testing.T) {
	root := t.TempDir()

	big := filepath.Join(root, "big.txt")
	f, err := os.Create(big)
	if err != nil {
		t.Fatalf("os.Create(%q) error = %v", big, err)
	}
	if err := f.Truncate(maxFileBytes + 1); err != nil {
		t.Fatalf("Truncate error = %v", err)
	}
	if _, err := f.WriteAt([]byte("needle"), 0); err != nil {
		t.Fatalf("WriteAt error = %v", err)
	}
	if err := f.Close(); err != nil {
		t.Fatalf("Close error = %v", err)
	}
	writeGlobFile(t, root, "small.txt", "needle small")

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "needle"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if strings.Contains(res.Text, "big.txt") {
		t.Fatalf("Text = %q, must not include the oversized file", res.Text)
	}
	if res.Text != "small.txt:1:needle small" {
		t.Fatalf("Text = %q, want %q", res.Text, "small.txt:1:needle small")
	}
}

func TestGrep_OverCap(t *testing.T) {
	root := t.TempDir()
	var b strings.Builder
	for i := 0; i < maxItems+5; i++ {
		fmt.Fprintf(&b, "needle line %d\n", i)
	}
	writeGlobFile(t, root, "many.txt", b.String())

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "needle"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}

	const notice = "\n[output truncated after 1000 matches]"
	if !strings.HasSuffix(res.Text, notice) {
		t.Fatalf("Text does not end with %q", notice)
	}
	content := strings.TrimSuffix(res.Text, notice)
	if got := len(strings.Split(content, "\n")); got != maxItems {
		t.Fatalf("got %d matches, want exactly %d", got, maxItems)
	}
}

func TestGrep_LongLineTruncates(t *testing.T) {
	root := t.TempDir()
	longLine := "needle " + strings.Repeat("x", maxOutputBytes+10)
	writeGlobFile(t, root, "long.txt", longLine)

	g := NewGrep(openFS(t, root))

	res := grepExecute(t, g, map[string]any{"pattern": "needle"})
	if res.IsError {
		t.Fatalf("IsError = true, want false; Text = %q", res.Text)
	}
	if res.Text == "no matches" {
		t.Fatalf("Text = %q, want a truncation notice rather than \"no matches\"", res.Text)
	}
	const notice = "\n[output truncated after 100 KiB]"
	if !strings.HasSuffix(res.Text, notice) {
		t.Fatalf("Text does not end with %q; got %q", notice, res.Text[:min(len(res.Text), 80)])
	}
}

func TestGrep_Confinement(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	writeGlobFile(t, outside, "secret.txt", "top secret payload")

	symlink := filepath.Join(root, "escape")
	if err := os.Symlink(outside, symlink); err != nil {
		t.Fatalf("os.Symlink error = %v", err)
	}

	g := NewGrep(openFS(t, root))

	t.Run("path escape via parent traversal", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "secret", "path": "../outside"})
		if !res.IsError {
			t.Fatalf("IsError = false, want true for an escaping path")
		}
	})

	t.Run("path escape via absolute outside", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "secret", "path": outside})
		if !res.IsError {
			t.Fatalf("IsError = false, want true for an escaping absolute path")
		}
	})

	t.Run("glob escape via parent traversal", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "secret", "glob": "../outside/*.txt"})
		if !res.IsError {
			t.Fatalf("IsError = false, want true for an escaping glob")
		}
	})

	t.Run("symlink inside pointing outside is not followed", func(t *testing.T) {
		res := grepExecute(t, g, map[string]any{"pattern": "top secret payload", "path": "escape"})
		if strings.Contains(res.Text, "top secret payload") {
			t.Fatalf("Text = %q, must not contain the external file's content regardless of IsError", res.Text)
		}
	})
}
