CREATE TABLE IF NOT EXISTS permission_rules (
  project TEXT NOT NULL,
  decision TEXT NOT NULL,
  name TEXT NOT NULL,
  argument TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  PRIMARY KEY(project, decision, name, argument)
);

CREATE INDEX IF NOT EXISTS permission_rules_project_idx ON permission_rules(project, created_at);
