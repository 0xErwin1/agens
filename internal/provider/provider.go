// Package provider defines the provider-neutral contract for talking to LLM
// backends: authentication, chat streaming, and explicit factory wiring.
// Depends only on internal/message and the standard library; contains no
// concrete provider, no HTTP calls, no credential persistence.
package provider

import (
	"context"
	"net/http"
	"time"
)

// Authenticator attaches provider credentials to outgoing HTTP requests and
// tracks whether those credentials are still usable.
//
// Decorate must be safe for concurrent use: multiple in-flight requests may
// call it at the same time. Refresh is invoked by the caller whenever Valid
// returns false; synchronizing concurrent Refresh calls and persisting any
// renewed credential are both owned by the implementation, not by this
// interface — Refresh returns no token or credential value. Long-lived
// credential persistence itself is out of scope here and owned by AGN-6.
type Authenticator interface {
	// Decorate attaches authentication material (for example an
	// Authorization header) to req before it is sent.
	Decorate(ctx context.Context, req *http.Request) error

	// Valid reports whether the current credential is usable at now,
	// without performing any network call.
	Valid(now time.Time) bool

	// Refresh renews the credential. Callers invoke it when Valid reports
	// false; a successful call must leave Valid true for at least the
	// caller's next Decorate.
	Refresh(ctx context.Context) error
}

// Provider is a single LLM backend: it identifies itself, reports the
// models it can serve, and opens streaming chat calls.
type Provider interface {
	// ID returns a stable, provider-scoped identifier (for example
	// "openai-api"), used for logging, multi-provider sessions, and error
	// wrapping.
	ID() string

	// Models lists the models this provider can serve.
	Models(ctx context.Context) ([]ModelInfo, error)

	// Stream opens a streaming chat call for req. The returned StreamReader
	// must be closed by the caller once consumption is done.
	Stream(ctx context.Context, req ChatRequest) (StreamReader, error)
}

// StreamReader delivers the incremental StreamEvent values of one Stream
// call.
//
// Recv returns io.EOF once the stream has ended cleanly, which happens
// after a StreamEvent with Type EventDone has already been delivered. Any
// other failure is reported solely through Recv's error return — a
// StreamEvent's fields are never used to signal an error. Close is
// idempotent and safe to call before the stream has been fully drained.
type StreamReader interface {
	// Recv returns the next StreamEvent, or io.EOF when the stream has
	// ended cleanly.
	Recv() (StreamEvent, error)

	// Close releases any resources held by the stream. It is safe to call
	// multiple times and safe to call before Recv has returned io.EOF.
	Close() error
}
