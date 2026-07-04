// Package chatgpt implements provider.Provider for OpenAI's Responses API
// ("/responses"), the streaming interface used by the ChatGPT product
// surface rather than the chat-completions API.
package chatgpt

import (
	"encoding/json"
	"fmt"
	"net/http"
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

// IsAuthError reports whether this response failed because of authentication:
// an HTTP 401 or 403. A "response.failed" envelope (StatusCode == 0) is a
// model/response failure, not a credential problem, so it is never classified
// as auth.
func (e *ResponseError) IsAuthError() bool {
	return e.StatusCode == http.StatusUnauthorized || e.StatusCode == http.StatusForbidden
}

// wireErrorEnvelope is the body of a non-2xx /responses response. The backend
// uses two shapes: a nested {"error":{code,message}} object and a flat
// {"detail":"..."} string; both are decoded so either surfaces a message.
type wireErrorEnvelope struct {
	Error struct {
		Code    string `json:"code"`
		Message string `json:"message"`
	} `json:"error"`
	Detail string `json:"detail"`
}

// parseResponseError parses body as a /responses error envelope, preferring
// error.message, then the flat detail field. If body is not valid JSON, or is
// JSON that carries none of those fields, Message falls back to a truncated
// raw snippet of body so the caller never sees an empty reason. It never
// returns nil, so callers can always attach the result via errors.As.
func parseResponseError(statusCode int, body []byte) error {
	var envelope wireErrorEnvelope
	if err := json.Unmarshal(body, &envelope); err == nil {
		message := envelope.Error.Message
		if message == "" {
			message = envelope.Detail
		}
		if message != "" || envelope.Error.Code != "" {
			return &ResponseError{
				StatusCode: statusCode,
				Code:       envelope.Error.Code,
				Message:    message,
			}
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
