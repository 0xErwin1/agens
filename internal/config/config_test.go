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
			name: "provider model default is empty (providers supply their own default)",
			check: func(t *testing.T, cfg Config) {
				if cfg.Provider.Model != "" {
					t.Fatalf("Provider.Model = %q, want empty", cfg.Provider.Model)
				}
			},
		},
		{
			name: "provider type default is empty (inferred from credentials)",
			check: func(t *testing.T, cfg Config) {
				if cfg.Provider.Type != "" {
					t.Fatalf("Provider.Type = %q, want empty", cfg.Provider.Type)
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
