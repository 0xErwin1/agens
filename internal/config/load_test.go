package config

import (
	"os"
	"path/filepath"
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
