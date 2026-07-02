package permission

import "testing"

func TestEvaluate(t *testing.T) {
	tests := []struct {
		name  string
		rules []Rule
		call  string
		arg   string
		want  Decision
	}{
		{
			name:  "empty ruleset defaults to ask",
			rules: nil,
			call:  "bash",
			arg:   "",
			want:  DecisionAsk,
		},
		{
			name: "no matching rule defaults to ask",
			rules: []Rule{
				{Decision: DecisionAllow, Name: "read_*"},
			},
			call: "bash",
			arg:  "",
			want: DecisionAsk,
		},
		{
			name: "multiple matches, last wins",
			rules: []Rule{
				{Decision: DecisionAllow, Name: "*"},
				{Decision: DecisionDeny, Name: "bash"},
			},
			call: "bash",
			arg:  "",
			want: DecisionDeny,
		},
		{
			name: "name-only rule matches regardless of argument",
			rules: []Rule{
				{Decision: DecisionAllow, Name: "read_*"},
			},
			call: "read_file",
			arg:  "anything",
			want: DecisionAllow,
		},
		{
			name: "name and argument glob requires both to match",
			rules: []Rule{
				{Decision: DecisionDeny, Name: "fs_write", Argument: "/etc/**"},
			},
			call: "fs_write",
			arg:  "/etc/passwd",
			want: DecisionDeny,
		},
		{
			name: "argument mismatch falls through to default ask",
			rules: []Rule{
				{Decision: DecisionDeny, Name: "fs_write", Argument: "/etc/**"},
			},
			call: "fs_write",
			arg:  "/home/x",
			want: DecisionAsk,
		},
		{
			name: "argument mismatch falls through to an earlier matching rule",
			rules: []Rule{
				{Decision: DecisionAllow, Name: "fs_write"},
				{Decision: DecisionDeny, Name: "fs_write", Argument: "/etc/**"},
			},
			call: "fs_write",
			arg:  "/home/x",
			want: DecisionAllow,
		},
		{
			name: "doublestar crosses a path separator",
			rules: []Rule{
				{Decision: DecisionAllow, Name: "fs_write", Argument: "src/**"},
			},
			call: "fs_write",
			arg:  "src/a/b.go",
			want: DecisionAllow,
		},
		{
			name: "single star does not cross a path separator",
			rules: []Rule{
				{Decision: DecisionAllow, Name: "fs_write", Argument: "src/*"},
			},
			call: "fs_write",
			arg:  "src/a/b.go",
			want: DecisionAsk,
		},
		{
			name: "empty argument glob matches any argument",
			rules: []Rule{
				{Decision: DecisionAllow, Name: "bash"},
			},
			call: "bash",
			arg:  "rm -rf /",
			want: DecisionAllow,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := evaluate(tt.rules, tt.call, tt.arg)
			if got != tt.want {
				t.Fatalf("evaluate(%+v, %q, %q) = %v, want %v", tt.rules, tt.call, tt.arg, got, tt.want)
			}

			gotAgain := evaluate(tt.rules, tt.call, tt.arg)
			if gotAgain != got {
				t.Fatalf("evaluate is not deterministic: first = %v, second = %v", got, gotAgain)
			}
		})
	}
}

func TestDecisionString(t *testing.T) {
	tests := []struct {
		d    Decision
		want string
	}{
		{DecisionAsk, "ask"},
		{DecisionAllow, "allow"},
		{DecisionDeny, "deny"},
		{Decision(99), "unknown"},
	}

	for _, tt := range tests {
		if got := tt.d.String(); got != tt.want {
			t.Fatalf("Decision(%d).String() = %q, want %q", tt.d, got, tt.want)
		}
	}
}
