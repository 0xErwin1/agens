package cli

import (
	"bytes"
	"strings"
	"testing"
)

func TestRootCommandHelp(t *testing.T) {
	cmd := NewRootCommand()
	buf := new(bytes.Buffer)
	cmd.SetOut(buf)
	cmd.SetErr(buf)
	cmd.SetArgs([]string{"--help"})

	if err := cmd.Execute(); err != nil {
		t.Fatalf("expected help to execute successfully: %v", err)
	}

	output := buf.String()
	for _, want := range []string{"Agens", "Usage:", "agens"} {
		if !strings.Contains(output, want) {
			t.Fatalf("expected help output to contain %q, got:\n%s", want, output)
		}
	}
}
