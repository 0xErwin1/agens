// Package message defines a provider-neutral, typed conversation history
// model. It has no dependency on Cobra or any provider package.
package message

import (
	"encoding/json"
	"fmt"
	"time"

	"github.com/google/uuid"
)

// Role identifies who authored a Message. The taxonomy is closed to exactly
// three values; there is no "tool" role, since tool results are carried by
// ToolResultPart inside user messages.
type Role string

const (
	RoleSystem    Role = "system"
	RoleUser      Role = "user"
	RoleAssistant Role = "assistant"
)

// Valid reports whether r is one of the closed set of known roles.
func (r Role) Valid() bool {
	switch r {
	case RoleSystem, RoleUser, RoleAssistant:
		return true
	default:
		return false
	}
}

// Message is a single turn in a conversation history.
type Message struct {
	ID         string    `json:"id"`
	Role       Role      `json:"role"`
	Parts      Parts     `json:"parts"`
	Model      string    `json:"model,omitempty"`
	StopReason string    `json:"stop_reason,omitempty"`
	CreatedAt  time.Time `json:"created_at"`
}

// NewMessage constructs a Message with a fresh UUID and the current UTC
// time. Optional metadata (Model, StopReason) is set by the caller
// afterward, since only assistant messages use it.
func NewMessage(role Role, parts ...Part) Message {
	return Message{
		ID:        uuid.NewString(),
		Role:      role,
		Parts:     parts,
		CreatedAt: time.Now().UTC(),
	}
}

// UnmarshalJSON decodes a Message and rejects an unknown Role explicitly
// rather than accepting it silently.
func (m *Message) UnmarshalJSON(data []byte) error {
	type alias Message
	var decoded alias
	if err := json.Unmarshal(data, &decoded); err != nil {
		return fmt.Errorf("decode message: %w", err)
	}
	if !decoded.Role.Valid() {
		return fmt.Errorf("decode message: unknown role %q", decoded.Role)
	}
	*m = Message(decoded)
	return nil
}
