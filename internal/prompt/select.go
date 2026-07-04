package prompt

import (
	"embed"
	"strings"
)

//go:embed prompts/*.txt
var promptFS embed.FS

const defaultPromptFile = "prompts/default.txt"

var familyPrompts = []struct {
	substr string
	file   string
}{
	{substr: "codex", file: "prompts/codex.txt"},
}

// Select returns the base system prompt text for a model id, chosen by
// ordered substring match against familyPrompts (first match wins),
// mirroring opencode's provider(modelID) dispatch. Add an entry to
// familyPrompts (and its prompts/*.txt file) to support another model
// family; anything that matches no entry falls through to default.txt.
func Select(modelID string) string {
	for _, family := range familyPrompts {
		if strings.Contains(modelID, family.substr) {
			return readPrompt(family.file)
		}
	}
	return readPrompt(defaultPromptFile)
}

func readPrompt(name string) string {
	data, err := promptFS.ReadFile(name)
	if err != nil {
		data, err = promptFS.ReadFile(defaultPromptFile)
		if err != nil {
			return ""
		}
	}
	return strings.TrimRight(string(data), " \t\r\n")
}
