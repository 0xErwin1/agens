// Package chatgpt implements provider.Provider for OpenAI's Responses API
// ("/responses"), the streaming interface used by the ChatGPT product
// surface rather than the chat-completions API.
package chatgpt

import (
	"encoding/json"
	"fmt"
)

// maxErrorSnippet bounds the raw body echoed into ResponseError.Message when
// the response body is not valid JSON.
const maxErrorSnippet = 200

// ResponseError represents a non-2xx /responses response, or a
// "response.failed"-style error envelope surfaced over the wire.
type ResponseError struct {
	StatusCode int
	Code       string
	Message    string
}

// Error implements the error interface. StatusCode == 0 identifies a
// "response.failed" SSE event surfaced mid-stream rather than a non-2xx HTTP
// response, so the HTTP-status portion of the message is omitted in that
// case to avoid the misleading "HTTP 0".
func (e *ResponseError) Error() string {
	if e.StatusCode == 0 {
		return fmt.Sprintf("chatgpt: response failed: %s (%s)", e.Message, e.Code)
	}
	return fmt.Sprintf("chatgpt: HTTP %d: %s (%s)", e.StatusCode, e.Message, e.Code)
}

// wireErrorEnvelope is the body of a non-2xx /responses response.
type wireErrorEnvelope struct {
	Error struct {
		Code    string `json:"code"`
		Message string `json:"message"`
	} `json:"error"`
}

// parseResponseError parses body as a /responses error envelope. If body is
// not valid JSON, Message falls back to a truncated raw snippet of body
// instead. It never returns nil, so callers can always attach the result via
// errors.As.
func parseResponseError(statusCode int, body []byte) error {
	var envelope wireErrorEnvelope
	if err := json.Unmarshal(body, &envelope); err == nil {
		return &ResponseError{
			StatusCode: statusCode,
			Code:       envelope.Error.Code,
			Message:    envelope.Error.Message,
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
