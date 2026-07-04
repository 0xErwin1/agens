package openai

import (
	"strings"
	"testing"
)

func TestResponseError_IsAuthError(t *testing.T) {
	cases := []struct {
		status int
		want   bool
	}{
		{401, true},
		{403, true},
		{500, false},
		{429, false},
	}
	for _, c := range cases {
		err := &ResponseError{StatusCode: c.status}
		if got := err.IsAuthError(); got != c.want {
			t.Fatalf("ResponseError{StatusCode:%d}.IsAuthError() = %v, want %v", c.status, got, c.want)
		}
	}
}

func TestParseResponseError_ValidEnvelope(t *testing.T) {
	body := []byte(`{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}`)

	err := parseResponseError(401, body)

	if err == nil {
		t.Fatal("parseResponseError() = nil, want a *ResponseError")
	}
	if err.StatusCode != 401 {
		t.Fatalf("StatusCode = %d, want 401", err.StatusCode)
	}
	if err.Message != "Incorrect API key provided" {
		t.Fatalf("Message = %q, want %q", err.Message, "Incorrect API key provided")
	}
	if err.Type != "invalid_request_error" {
		t.Fatalf("Type = %q, want %q", err.Type, "invalid_request_error")
	}
	if err.Code != "invalid_api_key" {
		t.Fatalf("Code = %q, want %q", err.Code, "invalid_api_key")
	}
}

func TestParseResponseError_NonJSONBodyUsesSnippet(t *testing.T) {
	body := []byte("<html>502 Bad Gateway</html>")

	err := parseResponseError(502, body)

	if err == nil {
		t.Fatal("parseResponseError() = nil, want a *ResponseError")
	}
	if err.StatusCode != 502 {
		t.Fatalf("StatusCode = %d, want 502", err.StatusCode)
	}
	if !strings.Contains(err.Message, "502 Bad Gateway") {
		t.Fatalf("Message = %q, want it to contain the raw body snippet", err.Message)
	}
}

func TestParseResponseError_NonJSONBodyTruncated(t *testing.T) {
	body := []byte(strings.Repeat("x", 10*1024))

	err := parseResponseError(500, body)

	if err == nil {
		t.Fatal("parseResponseError() = nil, want a *ResponseError")
	}
	if len(err.Message) >= len(body) {
		t.Fatalf("len(Message) = %d, want it truncated below %d", len(err.Message), len(body))
	}
}

func TestParseResponseError_NeverNil(t *testing.T) {
	if err := parseResponseError(500, nil); err == nil {
		t.Fatal("parseResponseError(500, nil) = nil, want a *ResponseError")
	}
	if err := parseResponseError(500, []byte{}); err == nil {
		t.Fatal("parseResponseError(500, []byte{}) = nil, want a *ResponseError")
	}
}

func TestResponseError_ErrorFormat(t *testing.T) {
	err := &ResponseError{
		StatusCode: 401,
		Message:    "Incorrect API key",
		Type:       "invalid_request_error",
		Code:       "invalid_api_key",
	}

	got := err.Error()
	if !strings.Contains(got, "401") || !strings.Contains(got, "Incorrect API key") || !strings.Contains(got, "invalid_api_key") {
		t.Fatalf("Error() = %q, want it to contain status, message, and code", got)
	}
}
