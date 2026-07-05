package agentdef

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestParse_FrontmatterAndBody(t *testing.T) {
	data := []byte("---\n" +
		"description: A focused researcher\n" +
		"mode: subagent\n" +
		"model: gpt-5.5\n" +
		"models:\n" +
		"  - gpt-5.5\n" +
		"  - gpt-4.1\n" +
		"---\n" +
		"You investigate things.\n")

	def, err := Parse("research", "research.md", data)
	if err != nil {
		t.Fatalf("Parse() error = %v, want nil", err)
	}

	if def.Name != "research" {
		t.Fatalf("Name = %q, want research", def.Name)
	}
	if def.Description != "A focused researcher" {
		t.Fatalf("Description = %q", def.Description)
	}
	if def.Mode != ModeSubagent {
		t.Fatalf("Mode = %q, want subagent", def.Mode)
	}
	if def.Model != "gpt-5.5" {
		t.Fatalf("Model = %q, want gpt-5.5", def.Model)
	}
	if !reflect.DeepEqual(def.Models, []string{"gpt-5.5", "gpt-4.1"}) {
		t.Fatalf("Models = %v, want [gpt-5.5 gpt-4.1]", def.Models)
	}
	if def.Prompt != "You investigate things." {
		t.Fatalf("Prompt = %q", def.Prompt)
	}
}

func TestParse_NoFrontmatterIsAllBody(t *testing.T) {
	def, err := Parse("plain", "plain.md", []byte("# Title\n\nJust a prompt.\n"))
	if err != nil {
		t.Fatalf("Parse() error = %v, want nil", err)
	}
	if def.Mode != ModeAll {
		t.Fatalf("Mode = %q, want the default all", def.Mode)
	}
	if !strings.Contains(def.Prompt, "Just a prompt.") {
		t.Fatalf("Prompt = %q, want the whole file as the body", def.Prompt)
	}
	if len(def.Models) != 0 {
		t.Fatalf("Models = %v, want none", def.Models)
	}
}

func TestParse_UnclosedFrontmatterIsBody(t *testing.T) {
	def, err := Parse("odd", "odd.md", []byte("---\ndescription: x\nno closing fence\n"))
	if err != nil {
		t.Fatalf("Parse() error = %v, want nil", err)
	}
	if def.Description != "" {
		t.Fatalf("Description = %q, want empty (the block was never closed)", def.Description)
	}
	if !strings.Contains(def.Prompt, "no closing fence") {
		t.Fatalf("Prompt = %q, want the raw content treated as body", def.Prompt)
	}
}

func TestParse_InvalidModeErrors(t *testing.T) {
	_, err := Parse("bad", "bad.md", []byte("---\nmode: sidekick\n---\nbody\n"))
	if err == nil {
		t.Fatal("Parse() error = nil, want an error for an unknown mode")
	}
	if !strings.Contains(err.Error(), "sidekick") {
		t.Fatalf("Parse() error = %v, want it to name the bad mode", err)
	}
}

func TestParse_InvalidYAMLErrors(t *testing.T) {
	_, err := Parse("bad", "bad.md", []byte("---\nmodels: [unterminated\n---\nbody\n"))
	if err == nil {
		t.Fatal("Parse() error = nil, want an error for malformed YAML frontmatter")
	}
}

func TestParse_ModelsAreTrimmedAndDeduped(t *testing.T) {
	def, err := Parse("m", "m.md", []byte("---\nmodels:\n  - gpt-5.5\n  - ' gpt-5.5 '\n  - ''\n  - gpt-4.1\n---\nbody\n"))
	if err != nil {
		t.Fatalf("Parse() error = %v, want nil", err)
	}
	if !reflect.DeepEqual(def.Models, []string{"gpt-5.5", "gpt-4.1"}) {
		t.Fatalf("Models = %v, want deduped and trimmed [gpt-5.5 gpt-4.1]", def.Models)
	}
}

func TestDefinition_AllowsModel(t *testing.T) {
	unrestricted := Definition{}
	if !unrestricted.AllowsModel("anything") {
		t.Fatal("an empty Models set must allow any model")
	}

	restricted := Definition{Models: []string{"gpt-5.5"}}
	if !restricted.AllowsModel("gpt-5.5") {
		t.Fatal("AllowsModel must accept a listed model")
	}
	if restricted.AllowsModel("gpt-4.1") {
		t.Fatal("AllowsModel must reject a model outside the set")
	}
}

func TestDefinition_ModeHelpers(t *testing.T) {
	if !(Definition{Mode: ModeAll}).IsSubagent() || !(Definition{Mode: ModeAll}).IsPrimary() {
		t.Fatal("ModeAll must satisfy both IsSubagent and IsPrimary")
	}
	if !(Definition{Mode: ModeSubagent}).IsSubagent() || (Definition{Mode: ModeSubagent}).IsPrimary() {
		t.Fatal("ModeSubagent must be a subagent but not primary")
	}
	if (Definition{Mode: ModePrimary}).IsSubagent() || !(Definition{Mode: ModePrimary}).IsPrimary() {
		t.Fatal("ModePrimary must be primary but not a subagent")
	}
}

func TestLoad_BuiltinsPresentWithoutFiles(t *testing.T) {
	set, err := Load("", "")
	if err != nil {
		t.Fatalf("Load() error = %v, want nil", err)
	}
	for _, name := range []string{"build", "plan"} {
		if _, ok := set.ByName(name); !ok {
			t.Fatalf("Load() missing built-in %q", name)
		}
	}
}

func TestLoad_MissingDirectoriesAreNotAnError(t *testing.T) {
	set, err := Load(filepath.Join(t.TempDir(), "nope"), filepath.Join(t.TempDir(), "nada"))
	if err != nil {
		t.Fatalf("Load() error = %v, want nil for missing directories", err)
	}
	if len(set.All()) != len(Builtins()) {
		t.Fatalf("Load() = %d defs, want just the built-ins", len(set.All()))
	}
}

func TestLoad_ProjectOverridesGlobalAndBuiltin(t *testing.T) {
	globalDir := t.TempDir()
	projectDir := t.TempDir()

	writeAgent(t, globalDir, "build.md", "---\ndescription: global build\n---\nglobal body\n")
	writeAgent(t, globalDir, "shared.md", "---\ndescription: from global\n---\nglobal shared\n")
	writeAgent(t, projectDir, "shared.md", "---\ndescription: from project\n---\nproject shared\n")

	set, err := Load(globalDir, projectDir)
	if err != nil {
		t.Fatalf("Load() error = %v, want nil", err)
	}

	build, _ := set.ByName("build")
	if build.Description != "global build" {
		t.Fatalf("build description = %q, want the global file to override the built-in", build.Description)
	}

	shared, _ := set.ByName("shared")
	if shared.Description != "from project" {
		t.Fatalf("shared description = %q, want the project file to override the global", shared.Description)
	}
}

func TestLoad_MalformedFileFailsFast(t *testing.T) {
	dir := t.TempDir()
	writeAgent(t, dir, "broken.md", "---\nmode: nonsense\n---\nbody\n")

	if _, err := Load(dir, ""); err == nil {
		t.Fatal("Load() error = nil, want a malformed agent file to fail fast")
	}
}

func TestSet_SubagentsExcludesPrimaryOnly(t *testing.T) {
	dir := t.TempDir()
	writeAgent(t, dir, "boss.md", "---\nmode: primary\n---\nboss\n")

	set, err := Load("", dir)
	if err != nil {
		t.Fatalf("Load() error = %v, want nil", err)
	}

	for _, d := range set.Subagents() {
		if d.Name == "boss" {
			t.Fatal("Subagents() included a primary-only agent, want it excluded")
		}
	}
}

func TestSaveModels_MaterializesBuiltinIntoProjectDir(t *testing.T) {
	projectDir := t.TempDir()

	build, _ := Load("", "")
	def, _ := build.ByName("build")

	path, err := SaveModels(projectDir, def, []string{"gpt-5.5", " gpt-5.5 ", "gpt-4.1"})
	if err != nil {
		t.Fatalf("SaveModels() error = %v, want nil", err)
	}
	if filepath.Dir(path) != projectDir {
		t.Fatalf("wrote to %s, want it inside the project dir %s", path, projectDir)
	}

	// The written file parses back with the deduped models and the body intact.
	reloaded, err := Load("", projectDir)
	if err != nil {
		t.Fatalf("reload error = %v", err)
	}
	got, ok := reloaded.ByName("build")
	if !ok {
		t.Fatal("saved build agent not found on reload")
	}
	if !reflect.DeepEqual(got.Models, []string{"gpt-5.5", "gpt-4.1"}) {
		t.Fatalf("reloaded Models = %v, want the deduped set", got.Models)
	}
	if got.Prompt != def.Prompt {
		t.Fatalf("reloaded prompt lost; got %q want %q", got.Prompt, def.Prompt)
	}
	if got.Source == sourceBuiltin {
		t.Fatal("reloaded build should be file-backed, want the project file to shadow the built-in")
	}
}

func TestSaveModels_RewritesFileBackedDefinitionInPlace(t *testing.T) {
	dir := t.TempDir()
	writeAgent(t, dir, "worker.md", "---\ndescription: does work\nmodels:\n  - gpt-4.1\n---\nthe worker prompt\n")

	set, _ := Load("", dir)
	def, _ := set.ByName("worker")

	path, err := SaveModels(dir, def, []string{"gpt-5.5"})
	if err != nil {
		t.Fatalf("SaveModels() error = %v, want nil", err)
	}
	if path != def.Source {
		t.Fatalf("wrote to %s, want the definition's own file %s", path, def.Source)
	}

	reloaded, _ := Load("", dir)
	got, _ := reloaded.ByName("worker")
	if !reflect.DeepEqual(got.Models, []string{"gpt-5.5"}) {
		t.Fatalf("reloaded Models = %v, want [gpt-5.5]", got.Models)
	}
	if got.Description != "does work" || got.Prompt != "the worker prompt" {
		t.Fatalf("rewrite lost fields: description=%q prompt=%q", got.Description, got.Prompt)
	}
}

func writeAgent(t *testing.T, dir, name, content string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(dir, name), []byte(content), 0o644); err != nil {
		t.Fatalf("write agent file %s: %v", name, err)
	}
}
