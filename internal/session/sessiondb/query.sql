-- name: UpsertSession :exec
INSERT INTO sessions (id, title, project, agent, updated_at)
VALUES (?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET
  title = excluded.title,
  project = excluded.project,
  agent = excluded.agent,
  updated_at = excluded.updated_at;

-- name: DeleteMessagesForSession :exec
DELETE FROM messages WHERE session_id = ?;

-- name: InsertMessage :exec
INSERT INTO messages (session_id, ordinal, message_id, role, model, stop_reason, created_at, payload_json)
VALUES (?, ?, ?, ?, ?, ?, ?, ?);

-- name: ListSessions :many
SELECT id, title, project, agent, updated_at
FROM sessions
ORDER BY updated_at DESC;

-- name: GetSession :one
SELECT id, title, project, agent, updated_at
FROM sessions
WHERE id = ?;

-- name: ListMessages :many
SELECT session_id, ordinal, message_id, role, model, stop_reason, created_at, payload_json
FROM messages
WHERE session_id = ?
ORDER BY ordinal ASC;
