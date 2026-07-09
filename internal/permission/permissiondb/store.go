// Package permissiondb persists permission.Rule grants ("allow always" /
// "deny always" answers) in a per-project SQLite database, so a grant a
// user answers once survives a restart without leaking to another project.
package permissiondb

import (
	"context"
	"database/sql"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/0xErwin1/agens/internal/permission"
	_ "modernc.org/sqlite"
)

const timeFormat = time.RFC3339Nano

// Store persists permission.Rule grants for one project in a SQLite
// database, implementing permission.Store. The project is bound at Open, so
// the Store interface signature stays unchanged while grants stay isolated
// per project.
type Store struct {
	path    string
	project string
	db      *sql.DB
	mu      sync.Mutex
}

var _ permission.Store = (*Store)(nil)

// Open returns a SQLite-backed permission Store scoped to project, using the
// database at path. The database file and schema are created lazily, on the
// first Append or Rules call, not here.
func Open(path, project string) (*Store, error) {
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, fmt.Errorf("permissiondb: open: %w", err)
	}
	db.SetMaxOpenConns(1)

	return &Store{path: path, project: project, db: db}, nil
}

// Close closes the underlying database handle.
func (s *Store) Close() error {
	return s.db.Close()
}

// Append persists r for the Store's project. A grant already recorded for
// the same (project, decision, name, argument) is a no-op, so re-answering
// the same Ask is idempotent.
func (s *Store) Append(ctx context.Context, r permission.Rule) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	if err := s.ensureReady(); err != nil {
		return err
	}

	_, err := s.db.ExecContext(ctx,
		`INSERT OR IGNORE INTO permission_rules (project, decision, name, argument, created_at) VALUES (?, ?, ?, ?, ?)`,
		s.project, r.Decision.String(), r.Name, r.Argument, formatTime(time.Now()),
	)
	if err != nil {
		return fmt.Errorf("permissiondb: append rule: %w", err)
	}
	return nil
}

// Rules returns the Store's project's persisted grants, oldest first.
func (s *Store) Rules(ctx context.Context) ([]permission.Rule, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if err := s.ensureReady(); err != nil {
		return nil, err
	}

	rows, err := s.db.QueryContext(ctx,
		`SELECT decision, name, argument FROM permission_rules WHERE project = ? ORDER BY created_at`,
		s.project,
	)
	if err != nil {
		return nil, fmt.Errorf("permissiondb: query rules: %w", err)
	}
	defer func() { _ = rows.Close() }()

	var rules []permission.Rule
	for rows.Next() {
		var decisionStr, name, argument string
		if err := rows.Scan(&decisionStr, &name, &argument); err != nil {
			return nil, fmt.Errorf("permissiondb: scan rule: %w", err)
		}
		decision, err := parseDecision(decisionStr)
		if err != nil {
			return nil, fmt.Errorf("permissiondb: %w", err)
		}
		rules = append(rules, permission.Rule{Decision: decision, Name: name, Argument: argument})
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("permissiondb: iterate rules: %w", err)
	}
	return rules, nil
}

// ensureReady applies the store's pragmas and schema, mirroring sessiondb:
// WAL journaling, NORMAL synchronous durability, a busy timeout so a
// concurrent reader never fails outright, and an idempotent
// CREATE-TABLE-IF-NOT-EXISTS migration stamped with PRAGMA user_version.
func (s *Store) ensureReady() error {
	if err := os.MkdirAll(filepath.Dir(s.path), 0o700); err != nil {
		return fmt.Errorf("permissiondb: create parent dir: %w", err)
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
			return fmt.Errorf("permissiondb: apply %s: %w", stmt, err)
		}
	}
	if _, err := s.db.ExecContext(ctx, schemaSQL); err != nil {
		return fmt.Errorf("permissiondb: initialize schema: %w", err)
	}
	if _, err := s.db.ExecContext(ctx, "PRAGMA user_version=1"); err != nil {
		return fmt.Errorf("permissiondb: set user_version: %w", err)
	}
	return nil
}

// parseDecision inverts permission.Decision.String, the format Append
// stores a Rule's Decision in.
func parseDecision(s string) (permission.Decision, error) {
	switch s {
	case "allow":
		return permission.DecisionAllow, nil
	case "deny":
		return permission.DecisionDeny, nil
	case "ask":
		return permission.DecisionAsk, nil
	default:
		return permission.DecisionAsk, fmt.Errorf("unknown decision %q", s)
	}
}

// DefaultPath is the permission database path: <data home>/agens/permissions.db.
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
	return filepath.Join(base, "agens", "permissions.db")
}

func formatTime(t time.Time) string {
	return t.UTC().Format(timeFormat)
}
