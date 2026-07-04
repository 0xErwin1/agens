package prompt

import (
	"strings"
	"testing"
)

func TestSelectReturnsCodexPromptForCodexModels(t *testing.T) {
	for _, modelID := range []string{"gpt-5-codex", "gpt-5.1-codex-max"} {
		got := Select(modelID)
		if got == "" {
			t.Fatalf("Select(%q) = empty, want non-empty", modelID)
		}
		if !strings.Contains(got, "running on a reasoning model") {
			t.Fatalf("Select(%q) = %q, want codex prompt content", modelID, got)
		}
	}
}

func TestSelectReturnsDefaultPromptForOtherModels(t *testing.T) {
	for _, modelID := range []string{"gpt-5.5", "gpt-4.1", "claude-x", ""} {
		got := Select(modelID)
		if got == "" {
			t.Fatalf("Select(%q) = empty, want non-empty", modelID)
		}
		if !strings.Contains(got, `Skip filler like "Sure, I can help with that."`) {
			t.Fatalf("Select(%q) = %q, want default prompt content", modelID, got)
		}
	}
}

func TestSelectTrimsTrailingWhitespace(t *testing.T) {
	got := Select("gpt-5.5")
	if got != strings.TrimRight(got, " \t\r\n") {
		t.Fatalf("Select() has untrimmed trailing whitespace: %q", got)
	}
}
