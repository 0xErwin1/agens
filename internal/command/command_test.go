package command

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestParseWithOptionalFrontmatter(t *testing.T) {
	cmd, err := Parse("review", "review.md", []byte("---\ndescription: Review code\nargument-hint: <path>\n---\nCheck $ARGUMENTS\n"))
	if err != nil {
		t.Fatalf("Parse() error = %v", err)
	}
	if cmd.Name != "review" || cmd.Description != "Review code" || cmd.ArgumentHint != "<path>" || cmd.Body != "Check $ARGUMENTS" || cmd.Source != "review.md" {
		t.Fatalf("Parse() = %#v", cmd)
	}

	plain, err := Parse("plain", "plain.md", []byte("Just do it\n"))
	if err != nil {
		t.Fatalf("Parse() plain error = %v", err)
	}
	if plain.Body != "Just do it" || plain.Description != "" || plain.ArgumentHint != "" {
		t.Fatalf("plain Parse() = %#v", plain)
	}
}

func TestParseRejectsInvalidNamesAndMalformedFrontmatter(t *testing.T) {
	if _, err := Parse("", "empty.md", []byte("body")); err == nil {
		t.Fatal("Parse() accepted empty name")
	}
	if _, err := Parse("bad/name", "bad.md", []byte("body")); err == nil {
		t.Fatal("Parse() accepted name with slash")
	}
	if _, err := Parse("bad", "bad.md", []byte("---\ndescription: [\n---\nbody")); err == nil {
		t.Fatal("Parse() accepted malformed frontmatter")
	}
}

func TestLoadOverlaysGlobalAndProjectMarkdown(t *testing.T) {
	root := t.TempDir()
	global := filepath.Join(root, "global")
	project := filepath.Join(root, "project")
	mustWrite(t, filepath.Join(global, "alpha.md"), "global alpha")
	mustWrite(t, filepath.Join(global, "shared.md"), "global shared")
	mustWrite(t, filepath.Join(global, "ignored.txt"), "nope")
	mustWrite(t, filepath.Join(project, "beta.md"), "project beta")
	mustWrite(t, filepath.Join(project, "shared.md"), "project shared")

	set, warnings := Load(global, project)
	if len(warnings) != 0 {
		t.Fatalf("Load() warnings = %v, want none", warnings)
	}

	got := namesAndBodies(set.All())
	want := []string{"alpha=global alpha", "shared=project shared", "beta=project beta"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("Load() = %v, want %v", got, want)
	}
}

func TestLoadMissingDirsAndMalformedFiles(t *testing.T) {
	root := t.TempDir()
	project := filepath.Join(root, "project")
	mustWrite(t, filepath.Join(project, "bad.md"), "---\ndescription: [\n---\nbody")
	mustWrite(t, filepath.Join(project, "good.md"), "body")

	set, warnings := Load(filepath.Join(root, "missing"), project)
	if set.Len() != 1 {
		t.Fatalf("Load() len = %d, want 1", set.Len())
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0].Error(), "bad.md") {
		t.Fatalf("Load() warnings = %v, want bad.md warning", warnings)
	}
}

func TestExpandReplacesArguments(t *testing.T) {
	cmd := Command{Body: "Review $ARGUMENTS now. $ARGUMENTS"}
	got := cmd.Expand("  src/main.go   and tests  ")
	want := "Review src/main.go   and tests now. src/main.go   and tests"
	if got != want {
		t.Fatalf("Expand() = %q, want %q", got, want)
	}
}

func mustWrite(t *testing.T, path, data string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(data), 0o644); err != nil {
		t.Fatal(err)
	}
}

func namesAndBodies(cmds []Command) []string {
	out := make([]string, 0, len(cmds))
	for _, cmd := range cmds {
		out = append(out, cmd.Name+"="+cmd.Body)
	}
	return out
}
