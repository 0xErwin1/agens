package permission

import (
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
)

// matcherSyntax splits a config matcher string into its tool name and,
// optionally, its parenthesized argument pattern: "bash(rm -rf *)" yields
// ("bash", "rm -rf *"); a bare "read" yields ("read", "").
var matcherSyntax = regexp.MustCompile(`^([^\s()]+)(?:\(([^)]*)\))?$`)

// ParseRule parses a config matcher string ("tool(argPattern)" or a bare
// "tool") into a Rule carrying decision, validating both the tool-name and
// argument doublestar patterns. It returns an error naming the offending
// pattern when the syntax is malformed or either pattern is not a valid
// doublestar glob, so an invalid [permissions] entry fails composition with
// a clear error instead of silently matching nothing.
func ParseRule(pattern string, decision Decision) (Rule, error) {
	m := matcherSyntax.FindStringSubmatch(pattern)
	if m == nil {
		return Rule{}, fmt.Errorf("permission: invalid matcher syntax %q", pattern)
	}

	rule := Rule{Decision: decision, Name: m[1], Argument: m[2]}
	if err := validateRule(rule); err != nil {
		return Rule{}, fmt.Errorf("permission: invalid matcher %q: %w", pattern, err)
	}
	return rule, nil
}

// ParseRules parses every entry of patterns with ParseRule, all under the
// same decision, and returns the first error encountered — so a caller
// composing a ruleset from a config bucket fails on the first invalid entry
// rather than silently dropping it.
func ParseRules(patterns []string, decision Decision) ([]Rule, error) {
	rules := make([]Rule, 0, len(patterns))
	for _, p := range patterns {
		rule, err := ParseRule(p, decision)
		if err != nil {
			return nil, err
		}
		rules = append(rules, rule)
	}
	return rules, nil
}

// projectionFields is the JSON shape ProjectField inspects to derive a
// call's semantic argument. Only one native tool's field is ever populated
// for a given call — bash carries "command", the filesystem tools carry
// "path", webfetch carries "url" — so checking them in a fixed order is
// equivalent to dispatching on the tool's name without needing it.
type projectionFields struct {
	Command string `json:"command"`
	Path    string `json:"path"`
	URL     string `json:"url"`
}

// ProjectField is the Projector every call site should use to derive the
// argument a matcher's Argument glob is matched against: a call's command,
// path, or url field, in that order, falling back to the raw JSON input
// verbatim when none is present. It consolidates the argument-extraction
// logic previously duplicated between the CLI and TUI prompters into the
// engine's single Projector seam.
func ProjectField(input json.RawMessage) string {
	var f projectionFields
	if err := json.Unmarshal(input, &f); err == nil {
		switch {
		case f.Command != "":
			return f.Command
		case f.Path != "":
			return f.Path
		case f.URL != "":
			return f.URL
		}
	}
	return string(input)
}

// literalGlobReplacer escapes every doublestar metacharacter with a leading
// backslash, in a single simultaneous pass so escaping the backslash itself
// never double-escapes a character it has already produced.
var literalGlobReplacer = strings.NewReplacer(
	`\`, `\\`,
	`*`, `\*`,
	`?`, `\?`,
	`[`, `\[`,
	`]`, `\]`,
	`{`, `\{`,
	`}`, `\}`,
)

// literalGlob escapes every doublestar metacharacter in s, so the result
// matches s and only s. It turns a persisted "remember this call" argument
// into a Rule.Argument glob that cannot accidentally widen to match a
// different call's argument.
func literalGlob(s string) string {
	return literalGlobReplacer.Replace(s)
}
