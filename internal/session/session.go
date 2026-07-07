// Package session persists agent conversations to disk so they can be listed
// and resumed. Each session is one JSON file holding its message history.
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
