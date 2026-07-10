package sessiondb

import (
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/0xErwin1/agens/internal/message"
	"github.com/0xErwin1/agens/internal/session"
	"github.com/0xErwin1/agens/internal/session/sessiondb/dbgen"
	_ "modernc.org/sqlite"
)

const timeFormat = time.RFC3339Nano

// Store persists sessions in a SQLite database using sqlc-generated queries.
type Store struct {
	path string
	db   *sql.DB
	q    *dbgen.Queries
	mu   sync.Mutex
}

// Open returns a SQLite-backed session store for path.
func Open(path string) (*Store, error) {
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, fmt.Errorf("sessiondb: open: %w", err)
	}
	db.SetMaxOpenConns(1)

	store := &Store{path: path, db: db, q: dbgen.New(db)}
	return store, nil
}

// Close closes the underlying database handle.
func (s *Store) Close() error {
	return s.db.Close()
}

// Save writes sess as a complete snapshot, stamping Updated with the current UTC time.
func (s *Store) Save(sess session.Session) error {
	if sess.ID == "" {
		return errors.New("sessiondb: save requires an ID")
	}

	s.mu.Lock()
	defer s.mu.Unlock()

	if err := s.ensureReady(); err != nil {
		return err
	}

	ctx := context.Background()
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return fmt.Errorf("sessiondb: begin save: %w", err)
	}
	committed := false
	defer func() {
		if !committed {
			_ = tx.Rollback()
		}
	}()

	queries := s.q.WithTx(tx)
	updated := time.Now().UTC()
	if err := queries.UpsertSession(ctx, dbgen.UpsertSessionParams{
		ID:        sess.ID,
		Title:     sess.Title,
		Project:   sess.Project,
		Agent:     sess.Agent,
		UpdatedAt: formatTime(updated),
	}); err != nil {
		return fmt.Errorf("sessiondb: upsert session %q: %w", sess.ID, err)
	}
	if err := queries.DeleteMessagesForSession(ctx, sess.ID); err != nil {
		return fmt.Errorf("sessiondb: replace messages %q: %w", sess.ID, err)
	}
	for i, msg := range sess.Messages {
		payload, err := json.Marshal(msg)
		if err != nil {
			return fmt.Errorf("sessiondb: marshal message %d for %q: %w", i, sess.ID, err)
		}
		if err := queries.InsertMessage(ctx, dbgen.InsertMessageParams{
			SessionID:   sess.ID,
			Ordinal:     int64(i),
			MessageID:   msg.ID,
			Role:        string(msg.Role),
			Model:       msg.Model,
			StopReason:  msg.StopReason,
			CreatedAt:   formatTime(msg.CreatedAt),
			PayloadJson: payload,
		}); err != nil {
			return fmt.Errorf("sessiondb: insert message %d for %q: %w", i, sess.ID, err)
		}
	}
	if err := tx.Commit(); err != nil {
		return fmt.Errorf("sessiondb: commit save %q: %w", sess.ID, err)
	}
	committed = true
	return nil
}

// List returns saved sessions ordered by most recent save first.
func (s *Store) List() ([]session.Session, error) {
	if err := s.ensureReady(); err != nil {
		return nil, err
	}

	rows, err := s.q.ListSessions(context.Background())
	if err != nil {
		return nil, fmt.Errorf("sessiondb: list sessions: %w", err)
	}

	sessions := make([]session.Session, 0, len(rows))
	for _, row := range rows {
		sess, err := s.sessionFromRow(row)
		if err != nil {
			continue
		}
		sessions = append(sessions, sess)
	}
	return sessions, nil
}

// ListMeta returns saved sessions' metadata (id, title, project, agent,
// updated_at) ordered by most recent save first, without loading or decoding
// any messages. Use Load to hydrate a single session's full history.
func (s *Store) ListMeta() ([]session.Session, error) {
	if err := s.ensureReady(); err != nil {
		return nil, err
	}

	rows, err := s.q.ListSessions(context.Background())
	if err != nil {
		return nil, fmt.Errorf("sessiondb: list sessions: %w", err)
	}

	sessions := make([]session.Session, 0, len(rows))
	for _, row := range rows {
		sess, err := sessionMetaFromRow(row)
		if err != nil {
			continue
		}
		sessions = append(sessions, sess)
	}
	return sessions, nil
}

// Delete removes the session with id and, via the messages table's FK
// CASCADE, its messages. Deleting a nonexistent id is a no-op: sqlc's :exec
// affecting zero rows still returns nil, so callers get idempotent deletes.
func (s *Store) Delete(id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	if err := s.ensureReady(); err != nil {
		return err
	}

	if err := s.q.DeleteSession(context.Background(), id); err != nil {
		return fmt.Errorf("sessiondb: delete %q: %w", id, err)
	}
	return nil
}

// Load reads the session with id.
func (s *Store) Load(id string) (session.Session, error) {
	if err := s.ensureReady(); err != nil {
		return session.Session{}, err
	}

	row, err := s.q.GetSession(context.Background(), id)
	if err != nil {
		return session.Session{}, fmt.Errorf("sessiondb: load %q: %w", id, err)
	}
	sess, err := s.sessionFromRow(row)
	if err != nil {
		return session.Session{}, fmt.Errorf("sessiondb: hydrate %q: %w", id, err)
	}
	return sess, nil
}

func (s *Store) ensureReady() error {
	if err := os.MkdirAll(filepath.Dir(s.path), 0o700); err != nil {
		return fmt.Errorf("sessiondb: create parent dir: %w", err)
	}
	ctx := context.Background()
	pragmas := []string{
		"PRAGMA foreign_keys=ON",
		"PRAGMA journal_mode=WAL",
		"PRAGMA synchronous=NORMAL",
		"PRAGMA busy_timeout=5000",
	}
	for _, stmt := range pragmas {
		if _, err := s.db.ExecContext(ctx, stmt); err != nil {
			return fmt.Errorf("sessiondb: apply %s: %w", stmt, err)
		}
	}
	if _, err := s.db.ExecContext(ctx, schemaSQL); err != nil {
		return fmt.Errorf("sessiondb: initialize schema: %w", err)
	}
	if _, err := s.db.ExecContext(ctx, "PRAGMA user_version=1"); err != nil {
		return fmt.Errorf("sessiondb: set user_version: %w", err)
	}
	return nil
}

func (s *Store) sessionFromRow(row dbgen.Session) (session.Session, error) {
	sess, err := sessionMetaFromRow(row)
	if err != nil {
		return session.Session{}, err
	}
	messages, err := s.loadMessages(row.ID)
	if err != nil {
		return session.Session{}, err
	}
	sess.Messages = messages
	return sess, nil
}

func sessionMetaFromRow(row dbgen.Session) (session.Session, error) {
	updated, err := parseTime(row.UpdatedAt)
	if err != nil {
		return session.Session{}, fmt.Errorf("decode updated_at: %w", err)
	}
	return session.Session{
		ID:      row.ID,
		Title:   row.Title,
		Project: row.Project,
		Agent:   row.Agent,
		Updated: updated,
	}, nil
}

func (s *Store) loadMessages(sessionID string) ([]message.Message, error) {
	rows, err := s.q.ListMessages(context.Background(), sessionID)
	if err != nil {
		return nil, fmt.Errorf("list messages: %w", err)
	}
	messages := make([]message.Message, 0, len(rows))
	for i, row := range rows {
		var msg message.Message
		if err := json.Unmarshal(row.PayloadJson, &msg); err != nil {
			return nil, fmt.Errorf("decode message %d: %w", i, err)
		}
		messages = append(messages, msg)
	}
	return messages, nil
}

// DefaultPath is the session database path: <data home>/agens/sessions.db.
func DefaultPath() string {
	base := os.Getenv("XDG_DATA_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil || home == "" {
			base = filepath.Join(".", ".local", "share")
		} else {
			base = filepath.Join(home, ".local", "share")
		}
	}
	return filepath.Join(base, "agens", "sessions.db")
}

func formatTime(t time.Time) string {
	return t.UTC().Format(timeFormat)
}

func parseTime(s string) (time.Time, error) {
	return time.Parse(timeFormat, s)
}
