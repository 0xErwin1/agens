package chatgpt

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"strings"
)

// idTokenClaims models the subset of an OpenAI id_token JWT payload needed
// to resolve the ChatGPT account id.
type idTokenClaims struct {
	ChatGPTAccountID string `json:"chatgpt_account_id"`
	Auth             *struct {
		ChatGPTAccountID string `json:"chatgpt_account_id"`
	} `json:"https://api.openai.com/auth"`
	Organizations []struct {
		ID string `json:"id"`
	} `json:"organizations"`
}

// jwtError reports a malformed or unresolvable id_token. It never includes
// the token value itself, so it is safe to log or wrap without leaking
// credentials.
type jwtError struct {
	reason string
}

func (e *jwtError) Error() string {
	return fmt.Sprintf("chatgpt: malformed id_token: %s", e.reason)
}

// parseAccountID extracts the ChatGPT account id from idToken's JWT
// payload WITHOUT verifying the token's signature. The token was just
// received directly from OpenAI's token endpoint over TLS, so this
// function self-consumes it for a single non-sensitive field rather than
// treating it as a verified assertion; callers must not rely on it for
// any authorization decision.
//
// The account id is resolved in order: (1) the top-level
// chatgpt_account_id claim, (2) chatgpt_account_id nested under the
// "https://api.openai.com/auth" claim, (3) the id of the first entry in
// organizations.
func parseAccountID(idToken string) (string, error) {
	segments := strings.Split(idToken, ".")
	if len(segments) < 2 {
		return "", &jwtError{reason: fmt.Sprintf("expected at least 2 dot-separated segments, got %d", len(segments))}
	}

	payload, err := base64.RawURLEncoding.DecodeString(segments[1])
	if err != nil {
		return "", &jwtError{reason: "payload is not valid base64url"}
	}

	var claims idTokenClaims
	if err := json.Unmarshal(payload, &claims); err != nil {
		return "", &jwtError{reason: "payload is not valid JSON"}
	}

	if claims.ChatGPTAccountID != "" {
		return claims.ChatGPTAccountID, nil
	}
	if claims.Auth != nil && claims.Auth.ChatGPTAccountID != "" {
		return claims.Auth.ChatGPTAccountID, nil
	}
	if len(claims.Organizations) > 0 && claims.Organizations[0].ID != "" {
		return claims.Organizations[0].ID, nil
	}

	return "", &jwtError{reason: "no chatgpt_account_id or organizations claim found"}
}
