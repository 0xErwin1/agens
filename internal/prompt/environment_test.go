package prompt

import (
	"strings"
	"testing"
	"time"
)

func TestEnvironmentRendersAllFields(t *testing.T) {
	now := time.Date(2026, 7, 4, 12, 0, 0, 0, time.UTC)
	got := Environment(Env{
		Model:       "gpt-5.5",
		WorkingDir:  "/home/user/project/sub",
		ProjectRoot: "/home/user/project",
		IsGitRepo:   true,
		Platform:    "linux",
		Now:         now,
	})

	for _, want := range []string{
		"You are powered by the model named gpt-5.5.",
		"Working directory: /home/user/project/sub",
		"Workspace root folder: /home/user/project",
		"Is directory a git repo: yes",
		"Platform: linux",
		"2026-07-04",
	} {
		if !strings.Contains(got, want) {
			t.Fatalf("Environment() = %q, want to contain %q", got, want)
		}
	}
}

func TestEnvironmentNonGitRepo(t *testing.T) {
	got := Environment(Env{
		Model:       "gpt-5.5",
		WorkingDir:  "/tmp/scratch",
		ProjectRoot: "/tmp/scratch",
		IsGitRepo:   false,
		Platform:    "darwin",
		Now:         time.Date(2026, 1, 2, 0, 0, 0, 0, time.UTC),
	})

	if !strings.Contains(got, "Is directory a git repo: no") {
		t.Fatalf("Environment() = %q, want %q", got, "Is directory a git repo: no")
	}
}

func TestEnvironmentOmitsPoweredByLineWhenModelIsEmpty(t *testing.T) {
	got := Environment(Env{
		Model:       "",
		WorkingDir:  "/tmp/scratch",
		ProjectRoot: "/tmp/scratch",
		IsGitRepo:   false,
		Platform:    "linux",
		Now:         time.Date(2026, 1, 2, 0, 0, 0, 0, time.UTC),
	})

	if strings.Contains(got, "You are powered by") {
		t.Fatalf("Environment() = %q, want no powered-by line", got)
	}
	if !strings.HasPrefix(got, "Here is some useful information") {
		t.Fatalf("Environment() = %q, want to start with the env block", got)
	}
}
