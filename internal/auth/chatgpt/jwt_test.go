package chatgpt

import (
	"encoding/base64"
	"errors"
	"strings"
	"testing"
)

func fakeIDTokenWithPayload(t *testing.T, payloadJSON string) string {
	t.Helper()
	header := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"none"}`))
	payload := base64.RawURLEncoding.EncodeToString([]byte(payloadJSON))
	return header + "." + payload + ".sig"
}

func TestParseAccountID_ResolutionOrder(t *testing.T) {
	tests := []struct {
		name        string
		payloadJSON string
		want        string
	}{
		{
			name:        "top-level claim",
			payloadJSON: `{"chatgpt_account_id":"acct-top"}`,
			want:        "acct-top",
		},
		{
			name:        "nested auth claim",
			payloadJSON: `{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-nested"}}`,
			want:        "acct-nested",
		},
		{
			name:        "organizations fallback",
			payloadJSON: `{"organizations":[{"id":"acct-org"}]}`,
			want:        "acct-org",
		},
		{
			name:        "top-level claim takes precedence over nested and organizations",
			payloadJSON: `{"chatgpt_account_id":"acct-top","https://api.openai.com/auth":{"chatgpt_account_id":"acct-nested"},"organizations":[{"id":"acct-org"}]}`,
			want:        "acct-top",
		},
		{
			name:        "nested auth claim takes precedence over organizations",
			payloadJSON: `{"https://api.openai.com/auth":{"chatgpt_account_id":"acct-nested"},"organizations":[{"id":"acct-org"}]}`,
			want:        "acct-nested",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, err := parseAccountID(fakeIDTokenWithPayload(t, tt.payloadJSON))
			if err != nil {
				t.Fatalf("parseAccountID() error = %v", err)
			}
			if got != tt.want {
				t.Fatalf("parseAccountID() = %q, want %q", got, tt.want)
			}
		})
	}
}

func TestParseAccountID_MalformedInput(t *testing.T) {
	tests := []struct {
		name  string
		token string
	}{
		{name: "single segment", token: "onlyoneseg"},
		{name: "bad base64 payload", token: "aGVhZGVy.not-base64-at-all!!!.sig"},
		{name: "bad json payload", token: fakeIDTokenWithPayload(t, "not-json")},
		{name: "no matching claim", token: fakeIDTokenWithPayload(t, "{}")},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, err := parseAccountID(tt.token)
			if err == nil {
				t.Fatalf("parseAccountID(%q) error = nil, want error", tt.token)
			}

			var jerr *jwtError
			if !errors.As(err, &jerr) {
				t.Fatalf("parseAccountID(%q) error type = %T, want *jwtError", tt.token, err)
			}
		})
	}
}

func TestParseAccountID_ErrorNeverLeaksToken(t *testing.T) {
	token := fakeIDTokenWithPayload(t, "not-json-super-secret-marker")

	_, err := parseAccountID(token)
	if err == nil {
		t.Fatal("parseAccountID() error = nil, want error for invalid JSON payload")
	}
	if strings.Contains(err.Error(), token) || strings.Contains(err.Error(), "super-secret-marker") {
		t.Fatalf("parseAccountID() error leaked token content: %v", err)
	}
}
