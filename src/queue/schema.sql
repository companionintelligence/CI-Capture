-- Local event queue schema. Bootstrapped on first open via PRAGMA user_version.
-- See docs/plan/02-file-watcher.md § 8.

PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS events (
  event_id          TEXT PRIMARY KEY,
  event_type        TEXT NOT NULL,
  payload_json      TEXT NOT NULL,
  occurred_at       TEXT NOT NULL,
  enqueued_at       TEXT NOT NULL,
  attempts          INTEGER NOT NULL DEFAULT 0,
  next_attempt_at   TEXT,                       -- NULL = ready to send
  last_error        TEXT
);

CREATE INDEX IF NOT EXISTS idx_events_ready
  ON events(next_attempt_at)
  WHERE next_attempt_at IS NULL;

-- Events the sync lane gave up on: the server permanently rejected them
-- (4xx / per-event rejection), or one event blocked the head of the queue
-- past the parking deadline. Parked, not dropped — nothing is deleted until
-- a human (or future UI) decides.
CREATE TABLE IF NOT EXISTS dead_letters (
  event_id          TEXT PRIMARY KEY,
  event_type        TEXT NOT NULL,
  payload_json      TEXT NOT NULL,
  occurred_at       TEXT NOT NULL,
  enqueued_at       TEXT NOT NULL,
  attempts          INTEGER NOT NULL,
  dead_lettered_at  TEXT NOT NULL,
  reason            TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS fingerprints (
  path          TEXT PRIMARY KEY,
  inode         INTEGER,
  size_bytes    INTEGER NOT NULL,
  mtime         TEXT NOT NULL,
  content_hash  TEXT NOT NULL,
  hashed_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS cursors (
  device_id            TEXT PRIMARY KEY,
  last_acked_event_id  TEXT,
  last_synced_at       TEXT
);

CREATE TABLE IF NOT EXISTS settings_overrides (
  key         TEXT PRIMARY KEY,
  value_json  TEXT NOT NULL
);
