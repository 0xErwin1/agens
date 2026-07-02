package agent

import (
	"strings"
	"testing"

	"github.com/iperez/agens/internal/auth"
	"github.com/iperez/agens/internal/config"
)

func validConfig() config.Config {
	cfg := config.DefaultConfig()
	cfg.Provider.Model = "gpt-4.1"
	cfg.Agent.SystemPrompt = "be helpful"
	return cfg
}

func validCreds() auth.File {
	return auth.File{
		defaultProviderID: {APIKey: "sk-test-key"},
	}
}

func TestBuildLoop_Success(t *testing.T) {
	loop, err := BuildLoop(validConfig(), validCreds(), Options{})
	if err != nil {
		t.Fatalf("BuildLoop() error = %v, want nil", err)
	}
	if loop == nil {
		t.Fatal("BuildLoop() loop = nil, want non-nil")
	}
}

func TestBuildLoop_ModelPrecedence(t *testing.T) {
	tests := []struct {
		name      string
		optsModel string
		cfgModel  string
		wantErr   bool
	}{
		{name: "opts overrides cfg", optsModel: "opt-model", cfgModel: "cfg-model", wantErr: false},
		{name: "falls back to cfg when opts empty", optsModel: "", cfgModel: "cfg-model", wantErr: false},
		{name: "errors when both empty", optsModel: "", cfgModel: "", wantErr: true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig()
			cfg.Provider.Model = tt.cfgModel

			loop, err := BuildLoop(cfg, validCreds(), Options{Model: tt.optsModel})

			if tt.wantErr {
				if err == nil {
					t.Fatal("BuildLoop() error = nil, want an error for an empty resolved model")
				}
				if !strings.Contains(err.Error(), "no model configured") {
					t.Fatalf("BuildLoop() error = %q, want it to mention %q", err.Error(), "no model configured")
				}
				return
			}
			if err != nil {
				t.Fatalf("BuildLoop() error = %v, want nil", err)
			}
			if loop == nil {
				t.Fatal("BuildLoop() loop = nil, want non-nil")
			}
		})
	}
}

func TestBuildLoop_SystemPromptPrecedence(t *testing.T) {
	tests := []struct {
		name    string
		optsSys string
		cfgSys  string
	}{
		{name: "opts overrides cfg", optsSys: "opts prompt", cfgSys: "cfg prompt"},
		{name: "falls back to cfg when opts empty", optsSys: "", cfgSys: "cfg prompt"},
		{name: "falls back to built-in default when both empty", optsSys: "", cfgSys: ""},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			cfg := validConfig()
			cfg.Agent.SystemPrompt = tt.cfgSys

			loop, err := BuildLoop(cfg, validCreds(), Options{SystemPrompt: tt.optsSys})
			if err != nil {
				t.Fatalf("BuildLoop() error = %v, want nil", err)
			}
			if loop == nil {
				t.Fatal("BuildLoop() loop = nil, want non-nil")
			}
		})
	}
}

func TestBuildLoop_MissingAPIKeyErrors(t *testing.T) {
	creds := auth.File{}

	_, err := BuildLoop(validConfig(), creds, Options{})
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error for missing credentials")
	}
	if !strings.Contains(err.Error(), defaultProviderID) {
		t.Fatalf("BuildLoop() error = %q, want it to name provider %q", err.Error(), defaultProviderID)
	}
}

func TestBuildLoop_EmptyAPIKeyErrors(t *testing.T) {
	creds := auth.File{
		defaultProviderID: {APIKey: ""},
	}

	_, err := BuildLoop(validConfig(), creds, Options{})
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error for an empty api_key")
	}
	if !strings.Contains(err.Error(), defaultProviderID) {
		t.Fatalf("BuildLoop() error = %q, want it to name provider %q", err.Error(), defaultProviderID)
	}
}

func TestBuildLoop_ErrorsNeverLeakAPIKeyValue(t *testing.T) {
	const secret = "sk-super-secret-value"
	creds := auth.File{
		"other-provider": {APIKey: secret},
	}

	_, err := BuildLoop(validConfig(), creds, Options{})
	if err == nil {
		t.Fatal("BuildLoop() error = nil, want an error since openai-api has no credentials")
	}
	if strings.Contains(err.Error(), secret) {
		t.Fatalf("BuildLoop() error = %q, must never contain a raw api_key value", err.Error())
	}
}
