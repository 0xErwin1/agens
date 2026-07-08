package config

import "testing"

func TestExpandEnv(t *testing.T) {
	tests := []struct {
		name string
		in   string
		env  map[string]string
		want string
	}{
		{name: "dollar", in: "$AGENS_VALUE/path", env: map[string]string{"AGENS_VALUE": "root"}, want: "root/path"},
		{name: "braces", in: "${AGENS_VALUE}/path", env: map[string]string{"AGENS_VALUE": "root"}, want: "root/path"},
		{name: "default", in: "${AGENS_MISSING:-fallback}/path", env: map[string]string{}, want: "fallback/path"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ExpandEnv(tt.in, tt.env)
			if err != nil {
				t.Fatalf("ExpandEnv() error = %v", err)
			}
			if got != tt.want {
				t.Fatalf("ExpandEnv() = %q, want %q", got, tt.want)
			}
		})
	}
}

func TestExpandEnvMissingVariableFails(t *testing.T) {
	_, err := ExpandEnv("$AGENS_MISSING", map[string]string{})
	if err == nil {
		t.Fatal("ExpandEnv() error = nil, want error")
	}
}

func TestExpandEnvDoesNotRunCommandsByDefault(t *testing.T) {
	got, err := ExpandEnv("$(printf should-not-run)", map[string]string{})
	if err != nil {
		t.Fatalf("ExpandEnv() error = %v", err)
	}
	if got != "$(printf should-not-run)" {
		t.Fatalf("ExpandEnv() = %q, want literal command expression", got)
	}
}

func TestExpandCommandSubstitutionHasNoStdin(t *testing.T) {
	got, err := ExpandEnvWithCommands("$(read value || printf no-stdin)", map[string]string{})
	if err != nil {
		t.Fatalf("ExpandEnvWithCommands() error = %v", err)
	}
	if got != "no-stdin" {
		t.Fatalf("ExpandEnvWithCommands() = %q, want no-stdin", got)
	}
}

func TestExpandCommandSubstitutionCapsOutput(t *testing.T) {
	_, err := ExpandEnvWithCommands("$(head -c 70000 /dev/zero | tr '\\0' x)", map[string]string{})
	if err == nil {
		t.Fatal("ExpandEnvWithCommands() error = nil, want output cap error")
	}
}
