package config

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestLoadDefaultsWhenConfigFilesAreMissing(t *testing.T) {
	home := t.TempDir()
	cwd := t.TempDir()

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: cwd, Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}

	if loaded.Config.Options.Debug {
		t.Fatalf("debug = true, want false")
	}
	if len(loaded.Sources) != 0 {
		t.Fatalf("sources = %v, want none", loaded.Sources)
	}
	if loaded.GlobalPath != filepath.Join(home, "config.toml") {
		t.Fatalf("global path = %q", loaded.GlobalPath)
	}
	if loaded.ProjectPath != filepath.Join(cwd, ".agens", "config.toml") {
		t.Fatalf("project path = %q", loaded.ProjectPath)
	}
}

func TestLoadProjectOverridesGlobal(t *testing.T) {
	home := t.TempDir()
	repo := t.TempDir()
	if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	projectConfigDir := filepath.Join(repo, ".agens")
	if err := os.Mkdir(projectConfigDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("[options]\ndebug = false\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(projectConfigDir, "config.toml"), []byte("[options]\ndebug = true\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: filepath.Join(repo, "nested"), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}

	if !loaded.Config.Options.Debug {
		t.Fatalf("debug = false, want true")
	}
	if loaded.ProjectRoot != repo {
		t.Fatalf("project root = %q, want %q", loaded.ProjectRoot, repo)
	}
	if len(loaded.Sources) != 2 {
		t.Fatalf("sources len = %d, want 2", len(loaded.Sources))
	}
}

func TestLoadUISectionDefaultsFalseAndOverrides(t *testing.T) {
	home := t.TempDir()

	def, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if def.Config.UI.CollapseThinking || def.Config.UI.TruncateToolOutput {
		t.Fatalf("UI defaults = %+v, want both false (shown in full)", def.Config.UI)
	}

	cfg := "[ui]\ncollapse_thinking = true\ntruncate_tool_output = true\n"
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(cfg), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if !loaded.Config.UI.CollapseThinking || !loaded.Config.UI.TruncateToolOutput {
		t.Fatalf("UI = %+v, want both true after override", loaded.Config.UI)
	}
}

func TestLoadRejectsMalformedTOML(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("[options\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	_, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err == nil {
		t.Fatal("LoadFrom() error = nil, want error")
	}
}

func TestLoadRejectsUnknownFields(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("unknown = true\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	_, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err == nil {
		t.Fatal("LoadFrom() error = nil, want error")
	}
}

func TestLoadExpandsDataDir(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("[options]\ndata_dir = \"$AGENS_DATA_ROOT/data\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{"AGENS_DATA_ROOT": "/tmp/agens"}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if loaded.Config.Options.DataDir != "/tmp/agens/data" {
		t.Fatalf("data dir = %q", loaded.Config.Options.DataDir)
	}
}

func TestLoadParsesProviderAndAgentSections(t *testing.T) {
	home := t.TempDir()
	toml := "[provider]\nmodel = \"gpt-4o\"\nbase_url = \"https://example.test\"\n\n[agent]\nsystem_prompt = \"X\"\nmax_iterations = 17\nparallel_tool_calls = false\n"
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(toml), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if loaded.Config.Provider.Model != "gpt-4o" {
		t.Fatalf("Provider.Model = %q, want %q", loaded.Config.Provider.Model, "gpt-4o")
	}
	if loaded.Config.Provider.BaseURL != "https://example.test" {
		t.Fatalf("Provider.BaseURL = %q, want %q", loaded.Config.Provider.BaseURL, "https://example.test")
	}
	if loaded.Config.Agent.SystemPrompt != "X" {
		t.Fatalf("Agent.SystemPrompt = %q, want %q", loaded.Config.Agent.SystemPrompt, "X")
	}
	if loaded.Config.Agent.MaxIterations != 17 {
		t.Fatalf("Agent.MaxIterations = %d, want 17", loaded.Config.Agent.MaxIterations)
	}
	if loaded.Config.Agent.ParallelToolCalls {
		t.Fatalf("Agent.ParallelToolCalls = true, want false")
	}
}

func TestLoadParsesProviderType(t *testing.T) {
	home := t.TempDir()
	toml := "[provider]\ntype = \"openai-chatgpt\"\n"
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(toml), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if loaded.Config.Provider.Type != "openai-chatgpt" {
		t.Fatalf("Provider.Type = %q, want %q", loaded.Config.Provider.Type, "openai-chatgpt")
	}
}

func TestLoadWithoutProviderOrAgentSectionsKeepsDefaults(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("[options]\ndebug = true\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	defaults := DefaultConfig()
	if loaded.Config.Provider.Model != defaults.Provider.Model {
		t.Fatalf("Provider.Model = %q, want default %q", loaded.Config.Provider.Model, defaults.Provider.Model)
	}
	if loaded.Config.Agent.SystemPrompt != defaults.Agent.SystemPrompt {
		t.Fatalf("Agent.SystemPrompt = %q, want default %q", loaded.Config.Agent.SystemPrompt, defaults.Agent.SystemPrompt)
	}
}

func TestLoadProjectOverridesGlobalProviderModel(t *testing.T) {
	home := t.TempDir()
	repo := t.TempDir()
	if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	projectConfigDir := filepath.Join(repo, ".agens")
	if err := os.Mkdir(projectConfigDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("[provider]\nmodel = \"a\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(projectConfigDir, "config.toml"), []byte("[provider]\nmodel = \"b\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: filepath.Join(repo, "nested"), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if loaded.Config.Provider.Model != "b" {
		t.Fatalf("Provider.Model = %q, want %q", loaded.Config.Provider.Model, "b")
	}
}

func TestLoadProjectOverridesGlobalAgentMaxIterations(t *testing.T) {
	home := t.TempDir()
	repo := t.TempDir()
	if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	projectConfigDir := filepath.Join(repo, ".agens")
	if err := os.Mkdir(projectConfigDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("[agent]\nmax_iterations = 11\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(projectConfigDir, "config.toml"), []byte("[agent]\nmax_iterations = 13\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: filepath.Join(repo, "nested"), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if loaded.Config.Agent.MaxIterations != 13 {
		t.Fatalf("Agent.MaxIterations = %d, want 13", loaded.Config.Agent.MaxIterations)
	}
}

func TestLoadExpandsBaseURLOnlyNotSystemPrompt(t *testing.T) {
	home := t.TempDir()
	toml := "[provider]\nbase_url = \"$AGENS_URL\"\n\n[agent]\nsystem_prompt = \"budget is $AGENS_URL literal\"\n"
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(toml), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{"AGENS_URL": "https://api.example.test"}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}
	if loaded.Config.Provider.BaseURL != "https://api.example.test" {
		t.Fatalf("Provider.BaseURL = %q, want expanded value", loaded.Config.Provider.BaseURL)
	}
	want := "budget is $AGENS_URL literal"
	if loaded.Config.Agent.SystemPrompt != want {
		t.Fatalf("Agent.SystemPrompt = %q, want literal %q", loaded.Config.Agent.SystemPrompt, want)
	}
}

func TestLoadPermissionsMergeSeparatesScopes(t *testing.T) {
	home := t.TempDir()
	repo := t.TempDir()
	if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	projectConfigDir := filepath.Join(repo, ".agens")
	if err := os.Mkdir(projectConfigDir, 0o755); err != nil {
		t.Fatal(err)
	}

	global := "[permissions]\nallow = [\"read(**)\"]\ndeny = [\"bash(rm -rf *)\"]\n"
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(global), 0o644); err != nil {
		t.Fatal(err)
	}
	project := "[permissions]\nallow = [\"bash(**)\"]\ndeny = [\"edit(**/.env)\"]\n"
	if err := os.WriteFile(filepath.Join(projectConfigDir, "config.toml"), []byte(project), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: repo, Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}

	perms := loaded.Config.Permissions
	if got, want := strings.Join(perms.GlobalAllow, ","), "read(**)"; got != want {
		t.Fatalf("GlobalAllow = %q, want %q", got, want)
	}
	if got, want := strings.Join(perms.GlobalDeny, ","), "bash(rm -rf *)"; got != want {
		t.Fatalf("GlobalDeny = %q, want %q", got, want)
	}
	if got, want := strings.Join(perms.ProjectAllow, ","), "bash(**)"; got != want {
		t.Fatalf("ProjectAllow = %q, want %q", got, want)
	}
	if got, want := strings.Join(perms.ProjectDeny, ","), "edit(**/.env)"; got != want {
		t.Fatalf("ProjectDeny = %q, want %q", got, want)
	}
}

func TestLoadPermissionsProjectAllowCannotReachGlobalDenyBucket(t *testing.T) {
	home := t.TempDir()
	repo := t.TempDir()
	if err := os.Mkdir(filepath.Join(repo, ".git"), 0o755); err != nil {
		t.Fatal(err)
	}
	projectConfigDir := filepath.Join(repo, ".agens")
	if err := os.Mkdir(projectConfigDir, 0o755); err != nil {
		t.Fatal(err)
	}

	global := "[permissions]\ndeny = [\"bash(rm -rf *)\"]\n"
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(global), 0o644); err != nil {
		t.Fatal(err)
	}
	project := "[permissions]\nallow = [\"bash(**)\"]\n"
	if err := os.WriteFile(filepath.Join(projectConfigDir, "config.toml"), []byte(project), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: repo, Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}

	perms := loaded.Config.Permissions
	if got, want := strings.Join(perms.GlobalDeny, ","), "bash(rm -rf *)"; got != want {
		t.Fatalf("GlobalDeny = %q, want %q (project allow must not reach the global deny bucket)", got, want)
	}
	if len(perms.ProjectDeny) != 0 {
		t.Fatalf("ProjectDeny = %v, want empty (project file set no deny)", perms.ProjectDeny)
	}
	if got, want := strings.Join(perms.ProjectAllow, ","), "bash(**)"; got != want {
		t.Fatalf("ProjectAllow = %q, want %q", got, want)
	}
}

func TestLoadWithoutPermissionsSectionKeepsBucketsEmpty(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte("[options]\ndebug = true\n"), 0o644); err != nil {
		t.Fatal(err)
	}

	loaded, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
	if err != nil {
		t.Fatalf("LoadFrom() error = %v", err)
	}

	perms := loaded.Config.Permissions
	if len(perms.GlobalAllow) != 0 || len(perms.GlobalDeny) != 0 || len(perms.ProjectAllow) != 0 || len(perms.ProjectDeny) != 0 {
		t.Fatalf("Permissions = %+v, want all buckets empty", perms)
	}
}

func TestLoadRejectsAgentMaxIterationsLessThanOne(t *testing.T) {
	for _, tc := range []struct {
		name  string
		value int
	}{
		{name: "zero", value: 0},
		{name: "negative", value: -1},
	} {
		t.Run(tc.name, func(t *testing.T) {
			home := t.TempDir()
			if err := os.WriteFile(filepath.Join(home, "config.toml"), []byte(fmt.Sprintf("[agent]\nmax_iterations = %d\n", tc.value)), 0o644); err != nil {
				t.Fatal(err)
			}

			_, err := LoadFrom(LoadOptions{ConfigHome: home, WorkingDir: t.TempDir(), Env: map[string]string{}})
			if err == nil {
				t.Fatalf("LoadFrom() error = nil, want an error for max_iterations = %d", tc.value)
			}
			if !strings.Contains(err.Error(), "max_iterations") {
				t.Fatalf("LoadFrom() error = %q, want it to mention max_iterations", err.Error())
			}
		})
	}
}
