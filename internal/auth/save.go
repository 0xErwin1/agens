package auth

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// Save atomically writes file as JSON to path. It creates the parent
// directory (mode 0700) if it does not already exist, writes to a
// temporary file in that same directory (mode 0600, per
// os.CreateTemp) and renames it into place, so a reader never
// observes a partially written credentials file. On any failure after
// the temporary file is created, Save removes it before returning.
func Save(path string, file File) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fmt.Errorf("auth: create directory %s: %w", dir, err)
	}

	tmp, err := os.CreateTemp(dir, ".auth-*.json")
	if err != nil {
		return fmt.Errorf("auth: create temporary credentials file: %w", err)
	}
	tmpPath := tmp.Name()

	data, err := json.Marshal(file)
	if err != nil {
		_ = tmp.Close()
		_ = os.Remove(tmpPath)
		return fmt.Errorf("auth: encode credentials: %w", err)
	}

	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		_ = os.Remove(tmpPath)
		return fmt.Errorf("auth: write credentials file: %w", err)
	}

	if err := tmp.Close(); err != nil {
		_ = os.Remove(tmpPath)
		return fmt.Errorf("auth: close credentials file: %w", err)
	}

	if err := os.Rename(tmpPath, path); err != nil {
		_ = os.Remove(tmpPath)
		return fmt.Errorf("auth: replace credentials file %s: %w", path, err)
	}

	return nil
}
