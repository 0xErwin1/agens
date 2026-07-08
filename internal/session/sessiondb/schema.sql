CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  project TEXT NOT NULL DEFAULT '',
  agent TEXT NOT NULL DEFAULT '',
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  ordinal INTEGER NOT NULL,
  message_id TEXT NOT NULL,
  role TEXT NOT NULL,
  model TEXT NOT NULL DEFAULT '',
  stop_reason TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  payload_json BLOB NOT NULL,
  PRIMARY KEY(session_id, ordinal)
);

CREATE INDEX IF NOT EXISTS sessions_updated_idx ON sessions(updated_at DESC);
