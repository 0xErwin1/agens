package prompt

import (
	"os"
	"path/filepath"
)

const maxInstructionBytes = 32 * 1024

var projectInstructionFilenames = []string{"AGENTS.md", "CLAUDE.md", "CONTEXT.md"}

// Instructions discovers AGENTS.md-style guidance files and returns each as
// "Instructions from: <path>\n<content>", in order (global first, then
// project). It reads at most maxInstructionBytes from each file.
//
// Only configHome is checked for global guidance (a top-level AGENTS.md);
// ~/.claude/CLAUDE.md is deliberately not read here, a departure from
// opencode, since that file holds personal assistant configuration rather
// than project instructions meant to be shared with any agent.
func Instructions(configHome, workingDir, projectRoot string) []string {
	var result []string

	if global, ok := readInstructionFile(filepath.Join(configHome, "AGENTS.md")); ok {
		result = append(result, global)
	}

	if project, ok := findProjectInstructions(workingDir, projectRoot); ok {
		result = append(result, project)
	}

	return result
}

// findProjectInstructions walks up from workingDir to projectRoot
// (inclusive) once per filename in projectInstructionFilenames priority
// order, and returns the content for the first filename that exists
// anywhere along that walk. This mirrors opencode's per-name findUp: a
// higher-priority filename found further up the tree still wins over a
// lower-priority filename found closer to workingDir, and only one project
// file is ever included.
func findProjectInstructions(workingDir, projectRoot string) (string, bool) {
	root, err := filepath.Abs(projectRoot)
	if err != nil {
		root = projectRoot
	}
	start, err := filepath.Abs(workingDir)
	if err != nil {
		start = workingDir
	}

	for _, name := range projectInstructionFilenames {
		current := start
		for {
			if content, ok := readInstructionFile(filepath.Join(current, name)); ok {
				return content, true
			}
			if current == root {
				break
			}
			parent := filepath.Dir(current)
			if parent == current {
				break
			}
			current = parent
		}
	}

	return "", false
}

func readInstructionFile(path string) (string, bool) {
	data, err := os.ReadFile(path)
	if err != nil {
		return "", false
	}
	if len(data) > maxInstructionBytes {
		data = data[:maxInstructionBytes]
	}
	return "Instructions from: " + path + "\n" + string(data), true
}
