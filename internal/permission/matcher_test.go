package permission

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/bmatcuk/doublestar/v4"
)

func TestParseRule(t *testing.T) {
	tests := []struct {
		name        string
		pattern     string
		decision    Decision
		wantRule    Rule
		wantErr     bool
		errContains string
	}{
		{
			name:     "tool with argument pattern",
			pattern:  "bash(rm -rf *)",
			decision: DecisionDeny,
			wantRule: Rule{Decision: DecisionDeny, Name: "bash", Argument: "rm -rf *"},
		},
		{
			name:     "bare tool matches on name alone",
			pattern:  "read",
			decision: DecisionAllow,
			wantRule: Rule{Decision: DecisionAllow, Name: "read", Argument: ""},
		},
		{
			name:     "empty parens are equivalent to a bare tool",
			pattern:  "read()",
			decision: DecisionAllow,
			wantRule: Rule{Decision: DecisionAllow, Name: "read", Argument: ""},
		},
		{
			name:     "doublestar glob argument",
			pattern:  "read(**/.env)",
			decision: DecisionDeny,
			wantRule: Rule{Decision: DecisionDeny, Name: "read", Argument: "**/.env"},
		},
		{
			name:     "mcp server-scoped tool name",
			pattern:  "engram_mem_save(**)",
			decision: DecisionAllow,
			wantRule: Rule{Decision: DecisionAllow, Name: "engram_mem_save", Argument: "**"},
		},
		{
			name:        "unclosed paren is invalid syntax",
			pattern:     "bash(rm -rf *",
			decision:    DecisionDeny,
			wantErr:     true,
			errContains: "bash(rm -rf *",
		},
		{
			name:        "trailing content after closing paren is invalid syntax",
			pattern:     "bash(rm)x",
			decision:    DecisionDeny,
			wantErr:     true,
			errContains: "bash(rm)x",
		},
		{
			name:        "invalid name glob",
			pattern:     "[",
			decision:    DecisionAllow,
			wantErr:     true,
			errContains: "[",
		},
		{
			name:        "invalid argument glob",
			pattern:     "bash([)",
			decision:    DecisionDeny,
			wantErr:     true,
			errContains: "bash([)",
		},
		{
			name:     "empty pattern is invalid syntax",
			pattern:  "",
			decision: DecisionAllow,
			wantErr:  true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := ParseRule(tt.pattern, tt.decision)
			if tt.wantErr {
				if err == nil {
					t.Fatalf("ParseRule(%q) error = nil, want non-nil", tt.pattern)
				}
				if tt.errContains != "" && !strings.Contains(err.Error(), tt.errContains) {
					t.Fatalf("ParseRule(%q) error = %q, want it to mention %q", tt.pattern, err.Error(), tt.errContains)
				}
				return
			}
			if err != nil {
				t.Fatalf("ParseRule(%q) error = %v, want nil", tt.pattern, err)
			}
			if got != tt.wantRule {
				t.Fatalf("ParseRule(%q) = %+v, want %+v", tt.pattern, got, tt.wantRule)
			}
		})
	}
}

func TestParseRules(t *testing.T) {
	got, err := ParseRules([]string{"bash(rm -rf *)", "read(**/.env)"}, DecisionDeny)
	if err != nil {
		t.Fatalf("ParseRules() error = %v, want nil", err)
	}
	want := []Rule{
		{Decision: DecisionDeny, Name: "bash", Argument: "rm -rf *"},
		{Decision: DecisionDeny, Name: "read", Argument: "**/.env"},
	}
	if len(got) != len(want) {
		t.Fatalf("ParseRules() = %+v, want %+v", got, want)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("ParseRules()[%d] = %+v, want %+v", i, got[i], want[i])
		}
	}
}

func TestParseRules_StopsOnFirstInvalidPattern(t *testing.T) {
	_, err := ParseRules([]string{"bash(**)", "["}, DecisionAllow)
	if err == nil {
		t.Fatalf("ParseRules() error = nil, want non-nil for a list containing an invalid matcher")
	}
}

func TestProjectField(t *testing.T) {
	tests := []struct {
		name  string
		input json.RawMessage
		want  string
	}{
		{
			name:  "bash call projects the command field",
			input: json.RawMessage(`{"command":"rm -rf /"}`),
			want:  "rm -rf /",
		},
		{
			name:  "fs call projects the path field",
			input: json.RawMessage(`{"path":"src/main.go"}`),
			want:  "src/main.go",
		},
		{
			name:  "webfetch call projects the url field",
			input: json.RawMessage(`{"url":"https://example.com"}`),
			want:  "https://example.com",
		},
		{
			name:  "command wins over path and url when several are present",
			input: json.RawMessage(`{"command":"ls","path":"src","url":"https://example.com"}`),
			want:  "ls",
		},
		{
			name:  "path wins over url when both are present",
			input: json.RawMessage(`{"path":"src","url":"https://example.com"}`),
			want:  "src",
		},
		{
			name:  "no known field falls back to raw input",
			input: json.RawMessage(`{"foo":"bar"}`),
			want:  `{"foo":"bar"}`,
		},
		{
			name:  "invalid JSON falls back to raw input",
			input: json.RawMessage(`not json`),
			want:  "not json",
		},
		{
			name:  "nil input falls back to empty string",
			input: nil,
			want:  "",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := ProjectField(tt.input); got != tt.want {
				t.Fatalf("ProjectField(%s) = %q, want %q", tt.input, got, tt.want)
			}
		})
	}
}

func TestLiteralGlob_EscapesMetacharacters(t *testing.T) {
	tests := []struct {
		name  string
		input string
	}{
		{name: "star", input: "rm -rf *"},
		{name: "question mark", input: "ls file?.go"},
		{name: "brackets", input: "echo [test]"},
		{name: "braces", input: "echo {a,b}"},
		{name: "backslash", input: `C:\path\to\file`},
		{name: "plain text has nothing to escape", input: "git status"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			escaped := literalGlob(tt.input)

			if !doublestar.ValidatePattern(escaped) {
				t.Fatalf("literalGlob(%q) = %q is not a valid doublestar pattern", tt.input, escaped)
			}
			if !doublestar.MatchUnvalidated(escaped, tt.input) {
				t.Fatalf("literalGlob(%q) = %q does not match its own source string", tt.input, escaped)
			}
		})
	}
}

func TestLiteralGlob_DoesNotWidenToOtherArguments(t *testing.T) {
	escaped := literalGlob("ls *.go")

	if doublestar.MatchUnvalidated(escaped, "ls main.go") {
		t.Fatalf("literalGlob(%q) = %q matched a different argument %q, want it to match only the literal source", "ls *.go", escaped, "ls main.go")
	}
	if !doublestar.MatchUnvalidated(escaped, "ls *.go") {
		t.Fatalf("literalGlob(%q) = %q did not match its own literal source", "ls *.go", escaped)
	}
}
