package provider

import "errors"

// AuthError is implemented by errors that represent an authentication or
// authorization failure: missing, invalid, or expired credentials. It lets
// higher layers (the TUI) recognize a credential problem and surface an
// actionable re-login hint without importing any concrete provider or auth
// package — the classification travels with the error itself.
type AuthError interface {
	error

	// IsAuthError reports whether this error is an authentication failure.
	// A single error type may be an auth failure only for certain states
	// (for example an HTTP error only for status 401/403), so the decision
	// is the type's own rather than implied by satisfying the interface.
	IsAuthError() bool
}

// IsAuthError reports whether err, or any error it wraps, is an authentication
// failure. It returns false for a nil error and for any error whose chain
// carries no AuthError.
func IsAuthError(err error) bool {
	var ae AuthError
	return errors.As(err, &ae) && ae.IsAuthError()
}
