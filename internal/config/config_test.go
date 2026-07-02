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

func TestDefaultConfigProviderAndAgent(t *testing.T) {
	tests := []struct {
		name  string
		check func(t *testing.T, cfg Config)
	}{
		{
			name: "provider model default",
			check: func(t *testing.T, cfg Config) {
				if cfg.Provider.Model != "gpt-4.1" {
					t.Fatalf("Provider.Model = %q, want %q", cfg.Provider.Model, "gpt-4.1")
				}
			},
		},
		{
			name: "agent system prompt default not empty",
			check: func(t *testing.T, cfg Config) {
				if cfg.Agent.SystemPrompt == "" {
					t.Fatal("Agent.SystemPrompt = \"\", want non-empty default")
				}
			},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := DefaultConfig()
			tt.check(t, cfg)
		})
	}
}
