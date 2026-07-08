// Package session defines saved conversation data that can be listed and resumed.
package session

import (
	"time"

	"github.com/0xErwin1/agens/internal/message"
)

// Session is one saved conversation. Project is the absolute project root the
// conversation belongs to, used to scope the session list per project; it is
// empty for sessions saved before per-project scoping existed, which surface
// only in the cross-project ("all") view.
type Session struct {
	ID       string            `json:"id"`
	Title    string            `json:"title"`
	Project  string            `json:"project,omitempty"`
	Agent    string            `json:"agent,omitempty"`
	Updated  time.Time         `json:"updated"`
	Messages []message.Message `json:"messages"`
}
