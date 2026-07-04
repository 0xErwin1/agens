package prompt

import (
	"fmt"
	"strings"
	"time"
)

type Env struct {
	Model       string
	WorkingDir  string
	ProjectRoot string
	IsGitRepo   bool
	Platform    string
	Now         time.Time
}

const envDateFormat = "2006-01-02"

// Environment renders the runtime context block appended after the base
// prompt. It states the real model (honest self-description, no invented
// persona) and the environment the agent is running in. When Model is
// empty, the "You are powered by" line is omitted rather than printed
// with a blank name.
func Environment(e Env) string {
	var b strings.Builder

	if e.Model != "" {
		fmt.Fprintf(&b, "You are powered by the model named %s.\n\n", e.Model)
	}

	b.WriteString("Here is some useful information about the environment you are running in:\n")
	b.WriteString("<env>\n")
	fmt.Fprintf(&b, "  Working directory: %s\n", e.WorkingDir)
	fmt.Fprintf(&b, "  Workspace root folder: %s\n", e.ProjectRoot)
	fmt.Fprintf(&b, "  Is directory a git repo: %s\n", gitRepoLabel(e.IsGitRepo))
	fmt.Fprintf(&b, "  Platform: %s\n", e.Platform)
	fmt.Fprintf(&b, "  Today's date: %s\n", e.Now.Format(envDateFormat))
	b.WriteString("</env>")

	return b.String()
}

func gitRepoLabel(isGitRepo bool) string {
	if isGitRepo {
		return "yes"
	}
	return "no"
}
