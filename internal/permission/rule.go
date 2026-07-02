// Package permission gates tool calls with an Allow|Ask|Deny decision
// resolved from a ruleset before the call executes.
package permission

import (
	"encoding/json"

	"github.com/bmatcuk/doublestar/v4"
)

// Decision is the outcome of evaluating a Call against a Ruleset.
type Decision int

const (
	// DecisionAsk is the zero value: the safe default when no rule matches.
	DecisionAsk Decision = iota
	DecisionAllow
	DecisionDeny
)

func (d Decision) String() string {
	switch d {
	case DecisionAllow:
		return "allow"
	case DecisionDeny:
		return "deny"
	case DecisionAsk:
		return "ask"
	default:
		return "unknown"
	}
}

// Rule is one entry in a ruleset: Decision applies when Name matches a
// call's tool name and, if Argument is non-empty, Argument also matches the
// call's projected argument.
type Rule struct {
	Decision Decision
	Name     string
	Argument string
}

// Projector derives the single string a Rule's Argument glob is matched
// against from a tool call's raw JSON input.
type Projector func(input json.RawMessage) string

// ProjectRaw is the default Projector: the raw JSON input verbatim.
func ProjectRaw(input json.RawMessage) string {
	return string(input)
}

// evaluate scans rules in order and returns the Decision of the last rule
// matching (name, arg), or DecisionAsk if none match. A rule with an empty
// Argument matches on name alone; a rule with a non-empty Argument matches
// only when both Name and Argument match.
func evaluate(rules []Rule, name, arg string) Decision {
	decision := DecisionAsk

	for _, r := range rules {
		if !doublestar.MatchUnvalidated(r.Name, name) {
			continue
		}
		if r.Argument != "" && !doublestar.MatchUnvalidated(r.Argument, arg) {
			continue
		}
		decision = r.Decision
	}

	return decision
}
