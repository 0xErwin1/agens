package prompt

import "strings"

// SkillInfo is the level-1 disclosure of one skill: its name and the
// description that states what it does and when to use it. This is all that is
// injected at startup; the full instructions are pulled on demand.
type SkillInfo struct {
	Name        string
	Description string
}

// skillsSection renders the level-1 skills block: a short instruction followed
// by one line per skill (name and description). It returns "" when there are no
// skills, so the section is dropped from the prompt entirely. The block tells
// the model to load a skill's full instructions with the skill tool before
// acting on it, which is how progressive disclosure advances to levels 2 and 3.
func skillsSection(skills []SkillInfo) string {
	if len(skills) == 0 {
		return ""
	}

	var b strings.Builder
	b.WriteString("# Available skills\n\n")
	b.WriteString("The following skills are available. Each line is a skill's name and a " +
		"description of what it does and when to use it. When a task matches a skill, call " +
		"the `skill` tool with that name to load its full instructions before acting on it.\n")

	for _, s := range skills {
		b.WriteString("\n- ")
		b.WriteString(s.Name)
		if s.Description != "" {
			b.WriteString(": ")
			b.WriteString(s.Description)
		}
	}

	return b.String()
}
