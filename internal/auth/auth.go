package auth

import (
	"encoding/json"
	"fmt"
	"os"
	"time"
)

// Entry holds the stored credentials for a single provider.
type Entry struct {
	APIKey       string     `json:"api_key,omitempty"`
	AccessToken  string     `json:"access_token,omitempty"`
	RefreshToken string     `json:"refresh_token,omitempty"`
	AccountID    string     `json:"account_id,omitempty"`
	ExpiresAt    *time.Time `json:"expires_at,omitempty"`
}

// File maps a provider id to its stored credentials.
type File map[string]Entry

// Load reads and parses the credentials file at path.
func Load(path string) (File, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("auth: credentials file %s: %w", path, err)
	}

	var file File
	if err := json.Unmarshal(data, &file); err != nil {
		return nil, fmt.Errorf("auth: credentials file %s: invalid JSON: %w", path, err)
	}
	return file, nil
}

// APIKey returns the API key stored for providerID, or an error naming the
// provider if no entry exists or the key is empty.
func (f File) APIKey(providerID string) (string, error) {
	entry, ok := f[providerID]
	if !ok {
		return "", fmt.Errorf("auth: no credentials for provider %q", providerID)
	}
	if entry.APIKey == "" {
		return "", fmt.Errorf("auth: no api_key for provider %q", providerID)
	}
	return entry.APIKey, nil
}
