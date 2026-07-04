// Package session persists agent conversations to disk so they can be listed
// and resumed. Each session is one JSON file holding its message history.
package session

import (
	"time"

	"github.com/iperez/agens/internal/message"
)

// Session is one saved conversation.
type Session struct {
	ID       string            `json:"id"`
	Title    string            `json:"title"`
	Updated  time.Time         `json:"updated"`
	Messages []message.Message `json:"messages"`
}
