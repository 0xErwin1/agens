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
