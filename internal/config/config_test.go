package config

import (
	"path/filepath"
	"testing"
)

func TestHomeDirUsesExplicitConfigHome(t *testing.T) {
	t.Setenv("AGENS_CONFIG_HOME", filepath.Join("tmp", "agens-config"))
	t.Setenv("XDG_CONFIG_HOME", filepath.Join("tmp", "xdg"))

	got := HomeDir()
	want := filepath.Join("tmp", "agens-config")
	if got != want {
		t.Fatalf("HomeDir() = %q, want %q", got, want)
	}
}

func TestHomeDirUsesXDGConfigHome(t *testing.T) {
	t.Setenv("AGENS_CONFIG_HOME", "")
	t.Setenv("XDG_CONFIG_HOME", filepath.Join("tmp", "xdg"))

	got := HomeDir()
	want := filepath.Join("tmp", "xdg", AppName)
	if got != want {
		t.Fatalf("HomeDir() = %q, want %q", got, want)
	}
}
