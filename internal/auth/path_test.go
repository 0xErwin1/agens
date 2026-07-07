package auth

import (
	"path/filepath"
	"testing"

	"github.com/0xErwin1/agens/internal/config"
)

func TestDefaultPathUsesExplicitConfigHome(t *testing.T) {
	t.Setenv("AGENS_CONFIG_HOME", filepath.Join("tmp", "agens-config"))
	t.Setenv("XDG_CONFIG_HOME", filepath.Join("tmp", "xdg"))

	got := DefaultPath()
	want := filepath.Join(config.HomeDir(), "auth.json")
	if got != want {
		t.Fatalf("DefaultPath() = %q, want %q", got, want)
	}
}

func TestDefaultPathUsesXDGConfigHomeWhenSet(t *testing.T) {
	t.Setenv("AGENS_CONFIG_HOME", "")
	t.Setenv("XDG_CONFIG_HOME", filepath.Join("tmp", "xdg"))

	got := DefaultPath()
	want := filepath.Join("tmp", "xdg", config.AppName, "auth.json")
	if got != want {
		t.Fatalf("DefaultPath() = %q, want %q", got, want)
	}
}
