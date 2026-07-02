package message

import (
	"encoding/json"
	"fmt"
)

// MarshalJSON writes each Part with its "type" discriminator embedded
// alongside the kind's own fields, so decode can dispatch back to the
// correct concrete kind.
func (p Parts) MarshalJSON() ([]byte, error) {
	raw := make([]json.RawMessage, len(p))
	for i, part := range p {
		data, err := marshalPart(part)
		if err != nil {
			return nil, fmt.Errorf("part %d: %w", i, err)
		}
		raw[i] = data
	}
	return json.Marshal(raw)
}

// UnmarshalJSON decodes each element by first reading its "type"
// discriminator and dispatching to the matching concrete kind. An unknown
// discriminator, a malformed element, or a disallowed nested kind is
// reported explicitly, wrapped with the element's index, rather than
// dropped or ignored.
func (p *Parts) UnmarshalJSON(data []byte) error {
	var rawParts []json.RawMessage
	if err := json.Unmarshal(data, &rawParts); err != nil {
		return fmt.Errorf("decode parts: %w", err)
	}

	decoded := make(Parts, len(rawParts))
	for i, raw := range rawParts {
		part, err := decodePart(raw)
		if err != nil {
			return fmt.Errorf("part %d: %w", i, err)
		}
		decoded[i] = part
	}

	*p = decoded
	return nil
}

func marshalPart(part Part) (json.RawMessage, error) {
	switch v := part.(type) {
	case TextPart:
		return json.Marshal(struct {
			Type string `json:"type"`
			TextPart
		}{v.Type(), v})
	case ToolUsePart:
		return json.Marshal(struct {
			Type string `json:"type"`
			ToolUsePart
		}{v.Type(), v})
	case ToolResultPart:
		if err := validateToolResultContent(v.Content); err != nil {
			return nil, err
		}
		return json.Marshal(struct {
			Type string `json:"type"`
			ToolResultPart
		}{v.Type(), v})
	default:
		return nil, fmt.Errorf("unsupported part kind %T", part)
	}
}

func decodePart(raw json.RawMessage) (Part, error) {
	var head struct {
		Type string `json:"type"`
	}
	if err := json.Unmarshal(raw, &head); err != nil {
		return nil, fmt.Errorf("decode part header: %w", err)
	}

	switch head.Type {
	case PartTypeText:
		var part TextPart
		if err := json.Unmarshal(raw, &part); err != nil {
			return nil, fmt.Errorf("decode text part: %w", err)
		}
		return part, nil
	case PartTypeToolUse:
		var part ToolUsePart
		if err := json.Unmarshal(raw, &part); err != nil {
			return nil, fmt.Errorf("decode tool_use part: %w", err)
		}
		return part, nil
	case PartTypeToolResult:
		var part ToolResultPart
		if err := json.Unmarshal(raw, &part); err != nil {
			return nil, fmt.Errorf("decode tool_result part: %w", err)
		}
		if err := validateToolResultContent(part.Content); err != nil {
			return nil, err
		}
		return part, nil
	default:
		return nil, fmt.Errorf("unknown part type %q", head.Type)
	}
}

// validateToolResultContent enforces that ToolResultPart.Content may only
// hold TextPart values (future ImagePart). Nested ToolUsePart or
// ToolResultPart values are rejected explicitly. It is called from both
// decodePart's tool_result branch and marshalPart's tool_result branch, so
// the invariant holds regardless of whether the value was built in memory
// or decoded from the wire.
func validateToolResultContent(content Parts) error {
	for i, part := range content {
		switch part.(type) {
		case TextPart:
		default:
			return fmt.Errorf("content %d: part kind %T is not allowed inside tool_result content", i, part)
		}
	}
	return nil
}
