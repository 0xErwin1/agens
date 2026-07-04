package tui

import "strings"

// internalErrorPrefixes are the package/context labels that Go's error wrapping
// (fmt.Errorf("pkg: ...: %w", err)) prepends as a failure propagates up through
// the agens packages. They carry no information for a user reading the chat, so
// they are stripped from the displayed message.
var internalErrorPrefixes = map[string]struct{}{
	"agens":     {},
	"agentloop": {},
	"provider":  {},
	"chatgpt":   {},
	"openai":    {},
	"auth":      {},
	"session":   {},
	"cli":       {},
	"tui":       {},
}

// humanizeError removes the internal package-name segments from an error
// message before it is shown to the user, turning a chain like
//
//	agentloop: open stream: chatgpt: HTTP 401: invalid api key (invalid_api_key)
//
// into
//
//	open stream: HTTP 401: invalid api key (invalid_api_key)
//
// It splits the message on ": ", drops any segment that is exactly an internal
// package label, and rejoins the rest, preserving descriptive segments and the
// innermost message. A message with no such segments is returned unchanged, and
// one made up entirely of labels falls back to the original so nothing is ever
// reduced to an empty string.
func humanizeError(msg string) string {
	segments := strings.Split(msg, ": ")

	kept := make([]string, 0, len(segments))
	for _, segment := range segments {
		if _, noise := internalErrorPrefixes[segment]; noise {
			continue
		}
		kept = append(kept, segment)
	}

	if len(kept) == 0 {
		return msg
	}
	return strings.Join(kept, ": ")
}
