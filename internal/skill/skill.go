// Package skill discovers Agent Skills — directories holding a SKILL.md with a
// name+description frontmatter and a markdown body — and exposes them to the
// agent through three levels of progressive disclosure: the name+description
// injected into the system prompt at startup (level 1), the full SKILL.md body
// (level 2), and bundled files under the skill's directory (level 3). The last
// two are read on demand through the built-in skill tool.
//
// The SKILL.md frontmatter follows the Anthropic Agent Skill contract (a
// required name of at most 64 chars and a required description of at most 1024),
// so a skill authored for Claude drops in unchanged.
package skill

// Skill is one discovered Agent Skill.
type Skill struct {
	Name        string // frontmatter name: the identifier the model loads it by
	Description string // frontmatter description: what it does and when to use it
	Body        string // the SKILL.md markdown body (level 2)
	Dir         string // the skill's directory, the root for bundled files (level 3)
	Source      string // path to the SKILL.md file, kept for diagnostics
}

// Set is an ordered, name-indexed collection of skills. A later put under an
// existing name overrides the earlier skill while keeping its position, so a
// project skill can shadow a global one of the same name without reordering.
type Set struct {
	skills []Skill
	index  map[string]int
}

func newSet() *Set {
	return &Set{index: make(map[string]int)}
}

func (s *Set) put(sk Skill) {
	if i, ok := s.index[sk.Name]; ok {
		s.skills[i] = sk
		return
	}
	s.index[sk.Name] = len(s.skills)
	s.skills = append(s.skills, sk)
}

// All returns every skill in discovery order.
func (s *Set) All() []Skill {
	out := make([]Skill, len(s.skills))
	copy(out, s.skills)
	return out
}

// ByName returns the skill registered under name, and whether it exists.
func (s *Set) ByName(name string) (Skill, bool) {
	i, ok := s.index[name]
	if !ok {
		return Skill{}, false
	}
	return s.skills[i], true
}

// Len reports how many skills the set holds.
func (s *Set) Len() int {
	return len(s.skills)
}
