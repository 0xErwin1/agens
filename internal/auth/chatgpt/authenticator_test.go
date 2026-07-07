package chatgpt

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/0xErwin1/agens/internal/auth"
)

func noopPersist(auth.Entry) error { return nil }

func timePtr(t time.Time) *time.Time { return &t }

func TestNewAuthenticator_RequiresRefreshTokenAndPersist(t *testing.T) {
	if _, err := NewAuthenticator(auth.Entry{}, noopPersist); err == nil {
		t.Fatal("NewAuthenticator() error = nil, want error for empty RefreshToken")
	}
	if _, err := NewAuthenticator(auth.Entry{RefreshToken: "rt-1"}, nil); err == nil {
		t.Fatal("NewAuthenticator() error = nil, want error for nil persist")
	}
	if _, err := NewAuthenticator(auth.Entry{RefreshToken: "rt-1"}, noopPersist); err != nil {
		t.Fatalf("NewAuthenticator() error = %v, want nil for a valid entry and persist", err)
	}
}

func TestAuthenticator_Valid(t *testing.T) {
	base := time.Date(2026, 1, 1, 12, 0, 0, 0, time.UTC)

	tests := []struct {
		name        string
		accessToken string
		expiresAt   *time.Time
		want        bool
	}{
		{
			name:        "well before expiry is valid",
			accessToken: "at-1",
			expiresAt:   timePtr(base.Add(time.Hour)),
			want:        true,
		},
		{
			name:        "exactly at the skew boundary is invalid",
			accessToken: "at-1",
			expiresAt:   timePtr(base.Add(refreshSkew)),
			want:        false,
		},
		{
			name:        "just inside the skew boundary is invalid",
			accessToken: "at-1",
			expiresAt:   timePtr(base.Add(refreshSkew - time.Second)),
			want:        false,
		},
		{
			name:        "just before the skew boundary is valid",
			accessToken: "at-1",
			expiresAt:   timePtr(base.Add(refreshSkew + time.Second)),
			want:        true,
		},
		{
			name:        "no ExpiresAt is invalid",
			accessToken: "at-1",
			expiresAt:   nil,
			want:        false,
		},
		{
			name:        "empty access token is invalid",
			accessToken: "",
			expiresAt:   timePtr(base.Add(time.Hour)),
			want:        false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			entry := auth.Entry{RefreshToken: "rt-1", AccessToken: tt.accessToken, ExpiresAt: tt.expiresAt}
			a, err := NewAuthenticator(entry, noopPersist)
			if err != nil {
				t.Fatal(err)
			}
			if got := a.Valid(base); got != tt.want {
				t.Fatalf("Valid(%v) = %v, want %v", base, got, tt.want)
			}
		})
	}
}

func TestAuthenticator_Decorate_SetsOnlyTwoHeadersWithExactCasing(t *testing.T) {
	entry := auth.Entry{
		RefreshToken: "rt-1",
		AccessToken:  "at-1",
		AccountID:    "acct-1",
		ExpiresAt:    timePtr(time.Now().Add(time.Hour)),
	}
	a, err := NewAuthenticator(entry, noopPersist)
	if err != nil {
		t.Fatal(err)
	}

	req := httptest.NewRequest(http.MethodGet, "https://example.invalid/", nil)
	if err := a.Decorate(context.Background(), req); err != nil {
		t.Fatalf("Decorate() error = %v, want nil", err)
	}

	if len(req.Header) != 2 {
		t.Fatalf("Decorate() set %d headers, want exactly 2: %v", len(req.Header), req.Header)
	}
	if got := req.Header.Get("Authorization"); got != "Bearer at-1" {
		t.Fatalf("Authorization header = %q, want %q", got, "Bearer at-1")
	}

	if got := req.Header.Get("ChatGPT-Account-ID"); got != "acct-1" {
		t.Fatalf(`Decorate() ChatGPT-Account-ID header = %q, want %q`, got, "acct-1")
	}
}

func TestAuthenticator_Refresh_ConcurrentCallersTriggerExactlyOnePOST(t *testing.T) {
	var postCount int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		atomic.AddInt32(&postCount, 1)
		_, _ = w.Write([]byte(`{"access_token":"at-new","refresh_token":"rt-new","id_token":"` + fakeIDToken(t, "acct-new") + `","expires_in":3600}`))
	}))
	defer server.Close()
	stubTokenEndpoint(t, server)

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	expired := fixedNow.Add(-time.Minute)
	entry := auth.Entry{RefreshToken: "rt-old", AccessToken: "at-old", AccountID: "acct-old", ExpiresAt: &expired}

	a, err := NewAuthenticator(entry, noopPersist)
	if err != nil {
		t.Fatal(err)
	}
	a.client = server.Client()
	a.now = func() time.Time { return fixedNow }

	const callers = 10
	var wg sync.WaitGroup
	errs := make([]error, callers)
	for i := range callers {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			errs[i] = a.Refresh(context.Background())
		}(i)
	}
	wg.Wait()

	for i, err := range errs {
		if err != nil {
			t.Fatalf("caller %d: Refresh() error = %v, want nil", i, err)
		}
	}
	if got := atomic.LoadInt32(&postCount); got != 1 {
		t.Fatalf("token endpoint received %d requests, want exactly 1", got)
	}

	a.mu.Lock()
	gotAccessToken := a.entry.AccessToken
	a.mu.Unlock()
	if gotAccessToken != "at-new" {
		t.Fatalf("entry.AccessToken after concurrent Refresh = %q, want %q", gotAccessToken, "at-new")
	}
}

func TestAuthenticator_Refresh_AdoptsRotatedRefreshTokenAndAccountID(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"access_token":"at-new","refresh_token":"rt-rotated","id_token":"` + fakeIDToken(t, "acct-new") + `","expires_in":3600}`))
	}))
	defer server.Close()
	stubTokenEndpoint(t, server)

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	expired := fixedNow.Add(-time.Minute)

	var persisted auth.Entry
	persist := func(e auth.Entry) error {
		persisted = e
		return nil
	}

	entry := auth.Entry{RefreshToken: "rt-old", AccessToken: "at-old", AccountID: "acct-old", ExpiresAt: &expired}
	a, err := NewAuthenticator(entry, persist)
	if err != nil {
		t.Fatal(err)
	}
	a.client = server.Client()
	a.now = func() time.Time { return fixedNow }

	if err := a.Refresh(context.Background()); err != nil {
		t.Fatalf("Refresh() error = %v, want nil", err)
	}

	if persisted.AccessToken != "at-new" {
		t.Fatalf("persisted.AccessToken = %q, want %q", persisted.AccessToken, "at-new")
	}
	if persisted.RefreshToken != "rt-rotated" {
		t.Fatalf("persisted.RefreshToken = %q, want the rotated token %q", persisted.RefreshToken, "rt-rotated")
	}
	if persisted.AccountID != "acct-new" {
		t.Fatalf("persisted.AccountID = %q, want %q", persisted.AccountID, "acct-new")
	}
	wantExpiresAt := fixedNow.Add(3600 * time.Second)
	if persisted.ExpiresAt == nil || !persisted.ExpiresAt.Equal(wantExpiresAt) {
		t.Fatalf("persisted.ExpiresAt = %v, want %v", persisted.ExpiresAt, wantExpiresAt)
	}
}

func TestAuthenticator_Refresh_KeepsExistingRefreshTokenAndAccountIDWhenOmitted(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"access_token":"at-new","expires_in":3600}`))
	}))
	defer server.Close()
	stubTokenEndpoint(t, server)

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	expired := fixedNow.Add(-time.Minute)

	var persisted auth.Entry
	persist := func(e auth.Entry) error {
		persisted = e
		return nil
	}

	entry := auth.Entry{RefreshToken: "rt-old", AccessToken: "at-old", AccountID: "acct-old", ExpiresAt: &expired}
	a, err := NewAuthenticator(entry, persist)
	if err != nil {
		t.Fatal(err)
	}
	a.client = server.Client()
	a.now = func() time.Time { return fixedNow }

	if err := a.Refresh(context.Background()); err != nil {
		t.Fatalf("Refresh() error = %v, want nil", err)
	}

	if persisted.AccessToken != "at-new" {
		t.Fatalf("persisted.AccessToken = %q, want %q", persisted.AccessToken, "at-new")
	}
	if persisted.RefreshToken != "rt-old" {
		t.Fatalf("persisted.RefreshToken = %q, want the retained old token %q", persisted.RefreshToken, "rt-old")
	}
	if persisted.AccountID != "acct-old" {
		t.Fatalf("persisted.AccountID = %q, want the retained old account id %q", persisted.AccountID, "acct-old")
	}
}

func TestAuthenticator_Refresh_MemoryUpdatedBeforePersistIsCalled(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"access_token":"at-new","refresh_token":"rt-new","id_token":"` + fakeIDToken(t, "acct-new") + `","expires_in":3600}`))
	}))
	defer server.Close()
	stubTokenEndpoint(t, server)

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	expired := fixedNow.Add(-time.Minute)
	entry := auth.Entry{RefreshToken: "rt-old", AccessToken: "at-old", AccountID: "acct-old", ExpiresAt: &expired}

	var a *Authenticator
	var inMemoryAtPersistTime auth.Entry
	persist := func(e auth.Entry) error {
		inMemoryAtPersistTime = a.entry
		return nil
	}

	built, err := NewAuthenticator(entry, persist)
	if err != nil {
		t.Fatal(err)
	}
	a = built
	a.client = server.Client()
	a.now = func() time.Time { return fixedNow }

	if err := a.Refresh(context.Background()); err != nil {
		t.Fatalf("Refresh() error = %v, want nil", err)
	}

	if inMemoryAtPersistTime.AccessToken != "at-new" {
		t.Fatalf("in-memory entry at persist time = %+v, want AccessToken %q already updated", inMemoryAtPersistTime, "at-new")
	}
}

func TestAuthenticator_Refresh_PersistFailureReturnsError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"access_token":"at-new","refresh_token":"rt-new","id_token":"` + fakeIDToken(t, "acct-new") + `","expires_in":3600}`))
	}))
	defer server.Close()
	stubTokenEndpoint(t, server)

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	expired := fixedNow.Add(-time.Minute)
	entry := auth.Entry{RefreshToken: "rt-old", AccessToken: "at-old", AccountID: "acct-old", ExpiresAt: &expired}

	persistErr := errors.New("disk full")
	a, err := NewAuthenticator(entry, func(auth.Entry) error { return persistErr })
	if err != nil {
		t.Fatal(err)
	}
	a.client = server.Client()
	a.now = func() time.Time { return fixedNow }

	err = a.Refresh(context.Background())
	if err == nil {
		t.Fatal("Refresh() error = nil, want an error when persist fails")
	}
	if !errors.Is(err, persistErr) {
		t.Fatalf("Refresh() error = %v, want it to wrap the persist error %v", err, persistErr)
	}
}

func TestAuthenticator_Refresh_HTTPFailureReturnsTypedErrorNotAbort(t *testing.T) {
	for _, status := range []int{http.StatusUnauthorized, http.StatusInternalServerError} {
		t.Run(http.StatusText(status), func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				w.WriteHeader(status)
			}))
			defer server.Close()
			stubTokenEndpoint(t, server)

			fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
			expired := fixedNow.Add(-time.Minute)
			entry := auth.Entry{RefreshToken: "rt-old", AccessToken: "at-old", AccountID: "acct-old", ExpiresAt: &expired}

			a, err := NewAuthenticator(entry, noopPersist)
			if err != nil {
				t.Fatal(err)
			}
			a.client = server.Client()
			a.now = func() time.Time { return fixedNow }

			err = a.Refresh(context.Background())
			var terr *tokenError
			if !errors.As(err, &terr) {
				t.Fatalf("Refresh() error type = %T, want *tokenError", err)
			}
		})
	}
}

func TestAuthenticator_Refresh_CtxCancelReturnsCtxError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"access_token":"at-new","expires_in":3600}`))
	}))
	defer server.Close()
	stubTokenEndpoint(t, server)

	fixedNow := time.Date(2026, 1, 1, 0, 0, 0, 0, time.UTC)
	expired := fixedNow.Add(-time.Minute)
	entry := auth.Entry{RefreshToken: "rt-old", AccessToken: "at-old", AccountID: "acct-old", ExpiresAt: &expired}

	a, err := NewAuthenticator(entry, noopPersist)
	if err != nil {
		t.Fatal(err)
	}
	a.client = server.Client()
	a.now = func() time.Time { return fixedNow }

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	err = a.Refresh(ctx)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("Refresh() error = %v, want context.Canceled", err)
	}
}
