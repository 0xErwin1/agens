package openai

import (
	"context"
	"net/http"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/provider"
)

var _ provider.Authenticator = (*APIKeyAuthenticator)(nil)

func TestNewAPIKeyAuthenticatorRejectsEmptyKey(t *testing.T) {
	auth, err := NewAPIKeyAuthenticator("")
	if err == nil {
		t.Fatal("NewAPIKeyAuthenticator(\"\") error = nil, want non-nil")
	}
	if auth != nil {
		t.Fatalf("NewAPIKeyAuthenticator(\"\") authenticator = %+v, want nil", auth)
	}
}

func TestAPIKeyAuthenticatorDecorateSetsAuthorizationHeader(t *testing.T) {
	auth, err := NewAPIKeyAuthenticator("sk-test")
	if err != nil {
		t.Fatalf("NewAPIKeyAuthenticator() error = %v, want nil", err)
	}

	req, err := http.NewRequest(http.MethodPost, "https://api.openai.com/v1/chat/completions", nil)
	if err != nil {
		t.Fatalf("http.NewRequest() error = %v", err)
	}

	if err := auth.Decorate(context.Background(), req); err != nil {
		t.Fatalf("Decorate() error = %v, want nil", err)
	}
	if got := req.Header.Get("Authorization"); got != "Bearer sk-test" {
		t.Fatalf("Authorization header = %q, want %q", got, "Bearer sk-test")
	}
}

func TestAPIKeyAuthenticatorValidIsAlwaysTrue(t *testing.T) {
	auth, err := NewAPIKeyAuthenticator("sk-test")
	if err != nil {
		t.Fatalf("NewAPIKeyAuthenticator() error = %v, want nil", err)
	}

	if !auth.Valid(time.Now()) {
		t.Fatal("Valid() = false, want true")
	}
}

func TestAPIKeyAuthenticatorRefreshIsNoOp(t *testing.T) {
	auth, err := NewAPIKeyAuthenticator("sk-test")
	if err != nil {
		t.Fatalf("NewAPIKeyAuthenticator() error = %v, want nil", err)
	}

	if err := auth.Refresh(context.Background()); err != nil {
		t.Fatalf("Refresh() error = %v, want nil", err)
	}
}
