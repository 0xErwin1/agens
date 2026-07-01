package message

import (
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/google/uuid"
)

func TestNewMessageAssignsUniqueValidUUIDs(t *testing.T) {
	first := NewMessage(RoleUser)
	second := NewMessage(RoleUser)

	if _, err := uuid.Parse(first.ID); err != nil {
		t.Fatalf("NewMessage() ID = %q, want valid UUID: %v", first.ID, err)
	}
	if _, err := uuid.Parse(second.ID); err != nil {
		t.Fatalf("NewMessage() ID = %q, want valid UUID: %v", second.ID, err)
	}
	if first.ID == second.ID {
		t.Fatalf("NewMessage() IDs = %q and %q, want distinct", first.ID, second.ID)
	}
}

func TestNewMessageSetsRoleAndUTCCreatedAt(t *testing.T) {
	before := time.Now().UTC()
	got := NewMessage(RoleAssistant)
	after := time.Now().UTC()

	if got.Role != RoleAssistant {
		t.Fatalf("NewMessage() Role = %q, want %q", got.Role, RoleAssistant)
	}
	if got.CreatedAt.Location() != time.UTC {
		t.Fatalf("NewMessage() CreatedAt location = %v, want UTC", got.CreatedAt.Location())
	}
	if got.CreatedAt.Before(before) || got.CreatedAt.After(after) {
		t.Fatalf("NewMessage() CreatedAt = %v, want between %v and %v", got.CreatedAt, before, after)
	}
}

func TestMessageMarshalOmitsEmptyOptionalFields(t *testing.T) {
	msg := Message{
		ID:    "test-id",
		Role:  RoleUser,
		Parts: Parts{},
	}

	data, err := json.Marshal(msg)
	if err != nil {
		t.Fatalf("json.Marshal() error = %v", err)
	}

	for _, key := range []string{"model", "stop_reason"} {
		if strings.Contains(string(data), `"`+key+`"`) {
			t.Fatalf("json.Marshal() = %s, want %q omitted", data, key)
		}
	}

	if !strings.Contains(string(data), `"created_at"`) {
		t.Fatalf("json.Marshal() = %s, want created_at always present", data)
	}
}

func TestMessageMarshalSerializesCreatedAtAsRFC3339(t *testing.T) {
	msg := NewMessage(RoleAssistant)

	data, err := json.Marshal(msg)
	if err != nil {
		t.Fatalf("json.Marshal() error = %v", err)
	}

	var decoded struct {
		CreatedAt string `json:"created_at"`
	}
	if err := json.Unmarshal(data, &decoded); err != nil {
		t.Fatalf("json.Unmarshal() error = %v", err)
	}
	if _, err := time.Parse(time.RFC3339, decoded.CreatedAt); err != nil {
		t.Fatalf("created_at = %q, want RFC3339: %v", decoded.CreatedAt, err)
	}
}

func TestMessageUnmarshalAcceptsKnownRoles(t *testing.T) {
	tests := []struct {
		name string
		role Role
	}{
		{"system", RoleSystem},
		{"user", RoleUser},
		{"assistant", RoleAssistant},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			input := `{"id":"test-id","role":"` + string(tt.role) + `","parts":[],"created_at":"2026-07-01T12:00:00Z"}`

			var got Message
			if err := json.Unmarshal([]byte(input), &got); err != nil {
				t.Fatalf("json.Unmarshal() error = %v", err)
			}
			if got.Role != tt.role {
				t.Fatalf("Role = %q, want %q", got.Role, tt.role)
			}
		})
	}
}

func TestMessageUnmarshalRejectsUnknownRole(t *testing.T) {
	input := `{"id":"test-id","role":"tool","parts":[],"created_at":"2026-07-01T12:00:00Z"}`

	var got Message
	err := json.Unmarshal([]byte(input), &got)
	if err == nil {
		t.Fatal("json.Unmarshal() error = nil, want explicit error for unknown role")
	}
}
