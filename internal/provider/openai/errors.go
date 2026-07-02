package openai

import (
	"encoding/json"
	"fmt"
)

// maxErrorSnippet bounds the raw body echoed into ResponseError.Message when
// the response body is not valid JSON.
const maxErrorSnippet = 200

// ResponseError represents a non-2xx chat-completions response.
type ResponseError struct {
	StatusCode int
	Message    string
	Type       string
	Code       string
}

func (e *ResponseError) Error() string {
	return fmt.Sprintf("openai: HTTP %d: %s (%s)", e.StatusCode, e.Message, e.Code)
}

// parseResponseError parses body as an OpenAI error envelope. If body is not
// valid JSON, Message falls back to a truncated raw snippet of body instead.
// It never returns nil, so callers can always attach the result via
// errors.As.
func parseResponseError(statusCode int, body []byte) *ResponseError {
	var envelope wireErrorEnvelope
	if err := json.Unmarshal(body, &envelope); err == nil {
		return &ResponseError{
			StatusCode: statusCode,
			Message:    envelope.Error.Message,
			Type:       envelope.Error.Type,
			Code:       envelope.Error.Code,
		}
	}

	snippet := string(body)
	if len(snippet) > maxErrorSnippet {
		snippet = snippet[:maxErrorSnippet]
	}

	return &ResponseError{
		StatusCode: statusCode,
		Message:    snippet,
	}
}
