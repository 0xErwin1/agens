package session

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// Store persists sessions as one JSON file per session under dir.
type Store struct {
	dir string
}

// NewStore returns a Store rooted at dir. The directory is created lazily on
// the first Save.
func NewStore(dir string) *Store {
	return &Store{dir: dir}
}

// Save writes sess to <dir>/<id>.json, stamping Updated with the current time,
// and creating the directory if needed. The write is atomic (temp file +
// rename) so a crash mid-write never corrupts an existing session.
func (s *Store) Save(sess Session) error {
	if sess.ID == "" {
		return errors.New("session: save requires an ID")
	}
	sess.Updated = time.Now()

	if err := os.MkdirAll(s.dir, 0o700); err != nil {
		return fmt.Errorf("session: create store dir: %w", err)
	}

	data, err := json.MarshalIndent(sess, "", "  ")
	if err != nil {
		return fmt.Errorf("session: marshal: %w", err)
	}

	path := s.path(sess.ID)
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return fmt.Errorf("session: write: %w", err)
	}
	if err := os.Rename(tmp, path); err != nil {
		return fmt.Errorf("session: rename: %w", err)
	}
	return nil
}

// List returns the saved sessions, most recently updated first. A missing
// store directory yields no sessions rather than an error; individual files
// that fail to decode are skipped so one corrupt file never hides the rest.
func (s *Store) List() ([]Session, error) {
	entries, err := os.ReadDir(s.dir)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return nil, nil
		}
		return nil, fmt.Errorf("session: read store dir: %w", err)
	}

	var sessions []Session
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}

		sess, err := s.Load(strings.TrimSuffix(entry.Name(), ".json"))
		if err != nil {
			continue
		}
		sessions = append(sessions, sess)
	}

	sort.Slice(sessions, func(i, j int) bool {
		return sessions[i].Updated.After(sessions[j].Updated)
	})
	return sessions, nil
}

// Load reads the session with the given id.
func (s *Store) Load(id string) (Session, error) {
	data, err := os.ReadFile(s.path(id))
	if err != nil {
		return Session{}, fmt.Errorf("session: read %q: %w", id, err)
	}

	var sess Session
	if err := json.Unmarshal(data, &sess); err != nil {
		return Session{}, fmt.Errorf("session: decode %q: %w", id, err)
	}
	return sess, nil
}

func (s *Store) path(id string) string {
	return filepath.Join(s.dir, id+".json")
}

// DefaultDir is the sessions directory: <data home>/agens/sessions, where the
// data home is XDG_DATA_HOME or ~/.local/share.
func DefaultDir() string {
	base := os.Getenv("XDG_DATA_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil || home == "" {
			base = filepath.Join(".", ".local", "share")
		} else {
			base = filepath.Join(home, ".local", "share")
		}
	}
	return filepath.Join(base, "agens", "sessions")
}
