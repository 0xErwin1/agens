package chatgpt

import (
	"strings"
	"testing"
)

func TestParseResponseError_ValidEnvelope(t *testing.T) {
	body := []byte(`{"error":{"message":"The model produced invalid output","code":"response_failed"}}`)

	err := parseResponseError(500, body)

	if err == nil {
		t.Fatal("parseResponseError() = nil, want a non-nil error")
	}

	var responseErr *ResponseError
	if !asResponseError(err, &responseErr) {
		t.Fatalf("parseResponseError() = %v (%T), want a *ResponseError", err, err)
	}
	if responseErr.StatusCode != 500 {
		t.Fatalf("StatusCode = %d, want 500", responseErr.StatusCode)
	}
	if responseErr.Message != "The model produced invalid output" {
		t.Fatalf("Message = %q, want %q", responseErr.Message, "The model produced invalid output")
	}
	if responseErr.Code != "response_failed" {
		t.Fatalf("Code = %q, want %q", responseErr.Code, "response_failed")
	}
}

func TestParseResponseError_NonJSONBodyUsesSnippet(t *testing.T) {
	body := []byte("<html>502 Bad Gateway</html>")

	err := parseResponseError(502, body)

	if err == nil {
		t.Fatal("parseResponseError() = nil, want a non-nil error")
	}
	if !strings.Contains(err.Error(), "502 Bad Gateway") {
		t.Fatalf("Error() = %q, want it to contain the raw body snippet", err.Error())
	}
}

func TestParseResponseError_NonJSONBodyTruncated(t *testing.T) {
	body := []byte(strings.Repeat("x", 10*1024))

	err := parseResponseError(500, body)

	var responseErr *ResponseError
	if !asResponseError(err, &responseErr) {
		t.Fatalf("parseResponseError() = %v (%T), want a *ResponseError", err, err)
	}
	if len(responseErr.Message) >= len(body) {
		t.Fatalf("len(Message) = %d, want it truncated below %d", len(responseErr.Message), len(body))
	}
}

func TestParseResponseError_NeverNil(t *testing.T) {
	if err := parseResponseError(500, nil); err == nil {
		t.Fatal("parseResponseError(500, nil) = nil, want a non-nil error")
	}
	if err := parseResponseError(500, []byte{}); err == nil {
		t.Fatal("parseResponseError(500, []byte{}) = nil, want a non-nil error")
	}
}

func TestResponseError_ErrorFormatContainsStatusAndMessageNoSecret(t *testing.T) {
	err := &ResponseError{
		StatusCode: 401,
		Code:       "invalid_api_key",
		Message:    "Incorrect API key provided",
	}

	got := err.Error()
	if !strings.Contains(got, "401") {
		t.Fatalf("Error() = %q, want it to contain the status code", got)
	}
	if !strings.Contains(got, "Incorrect API key provided") {
		t.Fatalf("Error() = %q, want it to contain the message", got)
	}
	if strings.Contains(got, "sk-") {
		t.Fatalf("Error() = %q, must never echo a secret-shaped token", got)
	}
}

func TestResponseError_ErrorFormat_StatusCodeZeroOmitsHTTPPrefix(t *testing.T) {
	err := &ResponseError{
		StatusCode: 0,
		Code:       "server_error",
		Message:    "boom",
	}

	got := err.Error()
	if strings.Contains(got, "HTTP 0") {
		t.Fatalf("Error() = %q, must not contain the misleading %q", got, "HTTP 0")
	}
	if !strings.Contains(got, "boom") {
		t.Fatalf("Error() = %q, want it to contain the message", got)
	}
	if !strings.Contains(got, "server_error") {
		t.Fatalf("Error() = %q, want it to contain the code", got)
	}
}

func TestResponseError_ErrorFormat_NonZeroStatusCodeStillUsesHTTPPrefix(t *testing.T) {
	err := &ResponseError{
		StatusCode: 429,
		Code:       "rate_limited",
		Message:    "slow down",
	}

	got := err.Error()
	if !strings.Contains(got, "HTTP 429") {
		t.Fatalf("Error() = %q, want it to still contain %q for a real non-2xx HTTP status", got, "HTTP 429")
	}
}

// asResponseError is a small local errors.As helper so the test package
// does not need to import "errors" solely for this one assertion.
func asResponseError(err error, target **ResponseError) bool {
	responseErr, ok := err.(*ResponseError)
	if !ok {
		return false
	}
	*target = responseErr
	return true
}
