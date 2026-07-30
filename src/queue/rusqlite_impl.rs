//! SQLite-backed durable event queue.
//!
//! Wraps a single `rusqlite::Connection` configured for WAL mode +
//! `synchronous = NORMAL`. The schema is bootstrapped from
//! [`schema.sql`](./schema.sql) on first open; subsequent opens are no-ops
//! because every statement is `CREATE … IF NOT EXISTS`.
//!
//! Concurrency model:
//!
//! - One owner per `Queue` (typically the sync engine).
//! - Producers send events through a tokio channel; the owner drains it
//!   into [`Queue::enqueue`]. This avoids cross-thread `Connection`
//!   sharing (which `rusqlite::Connection` doesn't support without a
//!   wrapper).
//!
//! See [`docs/plan/02-file-watcher.md`](../../../docs/plan/02-file-watcher.md) § 8
//! for the schema and retry policy.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::watcher::types::NormalizedEvent;

const SCHEMA_SQL: &str = include_str!("schema.sql");

/// One parked (dead-lettered) event: the event itself, plus why it parked.
#[derive(Debug, Clone)]
pub struct ParkedEvent {
    pub event_id: String,
    pub event: NormalizedEvent,
    pub reason: String,
}

/// One event sitting in the queue.
#[derive(Debug, Clone)]
pub struct QueuedEvent {
    pub event_id: String,
    pub event: NormalizedEvent,
    pub attempts: u32,
    /// When this event first failed to sync (None = never failed). Drives
    /// the head-of-queue parking deadline in the file tracker.
    pub first_failed_at: Option<DateTime<Utc>>,
}

pub struct Queue {
    conn: Connection,
}

impl Queue {
    /// Open (or create) the queue database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())
            .with_context(|| format!("opening queue db at {}", path.as_ref().display()))?;
        conn.execute_batch(SCHEMA_SQL)
            .context("bootstrapping queue schema")?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory queue. Useful for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory queue db")?;
        conn.execute_batch(SCHEMA_SQL)
            .context("bootstrapping queue schema")?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Insert one event. If the same `event_id` already exists, the
    /// insert is silently ignored — the producer is responsible for
    /// generating stable event ids, and re-inserting the same event after
    /// a crash should be a no-op.
    pub fn enqueue(&self, event: &NormalizedEvent) -> Result<()> {
        let payload = serde_json::to_string(event).context("serializing event payload")?;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO events
                    (event_id, event_type, payload_json, occurred_at,
                     enqueued_at, attempts, next_attempt_at, last_error)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, NULL)",
                params![
                    event.event_id,
                    format!("{:?}", event.event_type),
                    payload,
                    event.occurred_at.to_rfc3339(),
                    Utc::now().to_rfc3339(),
                ],
            )
            .context("inserting event")?;
        Ok(())
    }

    /// Take up to `max` events whose `next_attempt_at` has come due.
    /// Events stay in the table until [`Queue::ack`] is called — at-least-
    /// once delivery is the contract.
    pub fn take_batch(&self, max: usize) -> Result<Vec<QueuedEvent>> {
        let now = Utc::now().to_rfc3339();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT event_id, payload_json, attempts, first_failed_at
                 FROM events
                 WHERE next_attempt_at IS NULL OR next_attempt_at <= ?1
                 ORDER BY enqueued_at
                 LIMIT ?2",
            )
            .context("preparing take_batch")?;

        let rows = stmt
            .query_map(params![now, max as i64], |row| {
                let event_id: String = row.get(0)?;
                let payload_json: String = row.get(1)?;
                let attempts: i64 = row.get(2)?;
                let first_failed_at: Option<String> = row.get(3)?;
                Ok((event_id, payload_json, attempts, first_failed_at))
            })
            .context("executing take_batch")?;

        let mut out = Vec::new();
        for row in rows {
            let (event_id, payload_json, attempts, first_failed_at) =
                row.context("reading event row")?;
            let event: NormalizedEvent =
                serde_json::from_str(&payload_json).context("deserializing event payload")?;
            out.push(QueuedEvent {
                event_id,
                event,
                attempts: attempts.max(0) as u32,
                first_failed_at: first_failed_at
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&Utc)),
            });
        }
        Ok(out)
    }

    /// Permanently remove an event from the queue.
    pub fn ack(&self, event_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM events WHERE event_id = ?1", params![event_id])
            .context("ack")?;
        Ok(())
    }

    /// Mark an event as failed; schedule its next attempt after `delay`
    /// and record the error string. Caller chooses `delay` via the
    /// exponential-backoff policy in `sync::batching`. The first failure
    /// stamps `first_failed_at`, which later drives the parking deadline.
    pub fn requeue(&self, event_id: &str, delay: Duration, error: &str) -> Result<()> {
        let now = Utc::now();
        let next: DateTime<Utc> =
            now + chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::seconds(60));
        self.conn
            .execute(
                "UPDATE events
                 SET attempts = attempts + 1,
                     next_attempt_at = ?1,
                     last_error = ?2,
                     first_failed_at = COALESCE(first_failed_at, ?3)
                 WHERE event_id = ?4",
                params![next.to_rfc3339(), error, now.to_rfc3339(), event_id],
            )
            .context("requeue")?;
        Ok(())
    }

    /// Park an event in the `dead_letters` table and remove it from the
    /// live queue, in one transaction. Nothing is deleted — the event stays
    /// recoverable (and inspectable) on disk with the reason it was parked.
    pub fn dead_letter(&self, event_id: &str, reason: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let tx = self
            .conn
            .unchecked_transaction()
            .context("starting dead-letter transaction")?;
        tx.execute(
            "INSERT OR REPLACE INTO dead_letters
                (event_id, event_type, payload_json, occurred_at, enqueued_at,
                 attempts, dead_lettered_at, reason)
             SELECT event_id, event_type, payload_json, occurred_at, enqueued_at,
                    attempts, ?1, ?2
             FROM events WHERE event_id = ?3",
            params![now, reason, event_id],
        )
        .context("copying event to dead_letters")?;
        tx.execute("DELETE FROM events WHERE event_id = ?1", params![event_id])
            .context("removing dead-lettered event from queue")?;
        tx.commit().context("committing dead-letter transaction")?;
        Ok(())
    }

    /// How many events are parked in the dead-letter table.
    pub fn dead_letter_count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM dead_letters", [], |row| row.get(0))
            .context("dead_letter_count")?;
        Ok(n.max(0) as u64)
    }

    /// Every parked event, ordered by park time (ties — a coarse system
    /// clock can stamp two parks alike — are arbitrary).
    /// [`Queue::dead_letter_count`] answers "how many parked"; this answers
    /// *which* ones, and why — the only way a caller can tell that a specific
    /// event was parked rather than that some event was.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dead_letters(&self) -> Result<Vec<ParkedEvent>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT event_id, payload_json, reason
                 FROM dead_letters
                 ORDER BY dead_lettered_at",
            )
            .context("preparing dead_letters")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .context("executing dead_letters")?;

        let mut out = Vec::new();
        for row in rows {
            let (event_id, payload_json, reason) = row.context("reading dead-letter row")?;
            let event: NormalizedEvent = serde_json::from_str(&payload_json)
                .with_context(|| format!("deserializing parked event {event_id}"))?;
            out.push(ParkedEvent {
                event_id,
                event,
                reason,
            });
        }
        Ok(out)
    }

    /// How many events remain in the queue (across all states).
    pub fn depth(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .context("depth")?;
        Ok(n.max(0) as u64)
    }

    /// How many events have been attempted at least `n` times. Drives the
    /// dead-letter UX surface.
    pub fn attempts_over(&self, n: u32) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE attempts >= ?1",
                params![n as i64],
                |row| row.get(0),
            )
            .context("attempts_over")?;
        Ok(count.max(0) as u64)
    }

    /// Fetch the `last_error` recorded for `event_id`, if any.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn last_error(&self, event_id: &str) -> Result<Option<String>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT last_error FROM events WHERE event_id = ?1",
                params![event_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .context("last_error")?
            .flatten();
        Ok(row)
    }
}

/// In-place migrations for queues created by older builds. Everything here
/// must be idempotent — `open()` runs it on every start.
fn migrate(conn: &Connection) -> Result<()> {
    // v1 → v2: `first_failed_at` on events (drives the parking deadline).
    // schema.sql's CREATE TABLE IF NOT EXISTS won't touch existing tables,
    // so older databases need the column added by hand.
    let has_first_failed_at: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('events') WHERE name = 'first_failed_at'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .context("probing events schema")?;
    if !has_first_failed_at {
        conn.execute("ALTER TABLE events ADD COLUMN first_failed_at TEXT", [])
            .context("adding events.first_failed_at")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::watcher::types::{EventType, NormalizedEvent};

    fn evt(id: &str) -> NormalizedEvent {
        NormalizedEvent {
            event_id: id.to_string(),
            event_type: EventType::FileModified,
            occurred_at: Utc::now(),
            observed_at: Utc::now(),
            file: None,
            old_path: None,
            new_path: None,
            correlation_id: None,
        }
    }

    #[test]
    fn fresh_queue_is_empty() {
        let q = Queue::open_in_memory().unwrap();
        assert_eq!(q.depth().unwrap(), 0);
        assert!(q.take_batch(10).unwrap().is_empty());
    }

    #[test]
    fn enqueue_then_take_roundtrip() {
        let q = Queue::open_in_memory().unwrap();
        q.enqueue(&evt("a")).unwrap();
        q.enqueue(&evt("b")).unwrap();
        let batch = q.take_batch(10).unwrap();
        assert_eq!(batch.len(), 2);
        let ids: Vec<_> = batch.iter().map(|e| e.event_id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert_eq!(q.depth().unwrap(), 2);
    }

    #[test]
    fn ack_removes_from_queue() {
        let q = Queue::open_in_memory().unwrap();
        q.enqueue(&evt("a")).unwrap();
        q.ack("a").unwrap();
        assert_eq!(q.depth().unwrap(), 0);
        assert!(q.take_batch(10).unwrap().is_empty());
    }

    #[test]
    fn ack_unknown_id_is_a_noop() {
        let q = Queue::open_in_memory().unwrap();
        q.ack("never-existed").unwrap();
        assert_eq!(q.depth().unwrap(), 0);
    }

    #[test]
    fn duplicate_event_id_is_silently_ignored() {
        let q = Queue::open_in_memory().unwrap();
        q.enqueue(&evt("dupe")).unwrap();
        q.enqueue(&evt("dupe")).unwrap();
        assert_eq!(q.depth().unwrap(), 1);
    }

    #[test]
    fn requeue_increments_attempts_and_records_error() {
        let q = Queue::open_in_memory().unwrap();
        q.enqueue(&evt("a")).unwrap();
        q.requeue("a", Duration::from_secs(60), "boom").unwrap();
        q.requeue("a", Duration::from_secs(120), "boom2").unwrap();

        assert_eq!(q.depth().unwrap(), 1);

        // Future events shouldn't appear in take_batch yet.
        let batch = q.take_batch(10).unwrap();
        assert!(
            batch.is_empty(),
            "future-scheduled event leaked into batch: {:?}",
            batch
        );

        assert_eq!(q.attempts_over(2).unwrap(), 1);
        assert_eq!(q.attempts_over(3).unwrap(), 0);
        assert_eq!(q.last_error("a").unwrap().as_deref(), Some("boom2"));
    }

    #[test]
    fn past_due_requeue_comes_back() {
        let q = Queue::open_in_memory().unwrap();
        q.enqueue(&evt("a")).unwrap();
        // delay = 0 schedules next_attempt_at to roughly 'now'.
        q.requeue("a", Duration::from_secs(0), "boom").unwrap();
        let batch = q.take_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].event_id, "a");
        assert!(batch[0].attempts >= 1);
    }

    #[test]
    fn take_batch_respects_max() {
        let q = Queue::open_in_memory().unwrap();
        for i in 0..5 {
            q.enqueue(&evt(&format!("e{i}"))).unwrap();
        }
        let batch = q.take_batch(3).unwrap();
        assert_eq!(batch.len(), 3);
    }

    #[test]
    fn payload_survives_roundtrip() {
        let q = Queue::open_in_memory().unwrap();
        let mut e = evt("a");
        e.correlation_id = Some("group-7".to_string());
        e.event_type = EventType::FileRenamed;
        e.old_path = Some("/old/foo.txt".into());
        e.new_path = Some("/new/foo.txt".into());
        q.enqueue(&e).unwrap();
        let batch = q.take_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        let got = &batch[0].event;
        assert_eq!(got.correlation_id.as_deref(), Some("group-7"));
        assert_eq!(got.event_type, EventType::FileRenamed);
        assert_eq!(got.old_path.as_deref().unwrap().to_str(), Some("/old/foo.txt"));
        assert_eq!(got.new_path.as_deref().unwrap().to_str(), Some("/new/foo.txt"));
    }

    #[test]
    fn open_is_idempotent_on_disk() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        {
            let q = Queue::open(&path).unwrap();
            q.enqueue(&evt("survives")).unwrap();
            assert_eq!(q.depth().unwrap(), 1);
        }
        {
            let q = Queue::open(&path).unwrap();
            assert_eq!(q.depth().unwrap(), 1);
            let batch = q.take_batch(10).unwrap();
            assert_eq!(batch.len(), 1);
            assert_eq!(batch[0].event_id, "survives");
        }
    }
}

#[cfg(test)]
mod dead_letter_tests {
    use super::*;
    use crate::watcher::types::{EventType, NormalizedEvent};

    fn evt(id: &str) -> NormalizedEvent {
        NormalizedEvent {
            event_id: id.to_string(),
            event_type: EventType::FileModified,
            occurred_at: Utc::now(),
            observed_at: Utc::now(),
            file: None,
            old_path: None,
            new_path: None,
            correlation_id: None,
        }
    }

    #[test]
    fn dead_letter_parks_event_without_deleting_it() {
        let q = Queue::open_in_memory().unwrap();
        q.enqueue(&evt("poison")).unwrap();
        q.requeue("poison", Duration::from_secs(0), "HTTP 400").unwrap();

        q.dead_letter("poison", "event ingest rejected event 0: bad payload")
            .unwrap();

        // Gone from the live queue…
        assert_eq!(q.depth().unwrap(), 0);
        assert!(q.take_batch(10).unwrap().is_empty());
        // …but parked, not deleted.
        assert_eq!(q.dead_letter_count().unwrap(), 1);
        let (reason, attempts): (String, i64) = q
            .conn
            .query_row(
                "SELECT reason, attempts FROM dead_letters WHERE event_id = 'poison'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(reason.contains("bad payload"));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn dead_letters_identify_which_events_parked() {
        let q = Queue::open_in_memory().unwrap();
        for id in ["keeper", "poison", "other-poison"] {
            q.enqueue(&evt(id)).unwrap();
        }
        q.dead_letter("poison", "HTTP 400: bad payload").unwrap();
        q.dead_letter("other-poison", "HTTP 422: schema").unwrap();

        let parked = q.dead_letters().unwrap();
        assert_eq!(parked.len(), 2);
        // Look rows up by id rather than by position: a coarse system clock
        // can stamp both parks with the same `dead_lettered_at`, leaving the
        // order arbitrary.
        let poison = parked.iter().find(|p| p.event_id == "poison").unwrap();
        // The payload round-trips, so callers can inspect the event itself.
        assert_eq!(poison.event.event_id, "poison");
        assert!(poison.reason.contains("bad payload"));
        assert!(
            parked.iter().all(|p| p.event_id != "keeper"),
            "an un-parked event must not appear in dead_letters"
        );
    }

    #[test]
    fn dead_letter_unknown_id_is_a_noop() {
        let q = Queue::open_in_memory().unwrap();
        q.dead_letter("never-existed", "why not").unwrap();
        assert_eq!(q.dead_letter_count().unwrap(), 0);
    }

    #[test]
    fn first_failed_at_set_once_and_survives_roundtrip() {
        let q = Queue::open_in_memory().unwrap();
        q.enqueue(&evt("a")).unwrap();

        // Never failed → None.
        let batch = q.take_batch(10).unwrap();
        assert!(batch[0].first_failed_at.is_none());

        q.requeue("a", Duration::from_secs(0), "boom").unwrap();
        let first = q.take_batch(10).unwrap()[0].first_failed_at.unwrap();

        // Second failure must NOT move the first-failure stamp.
        q.requeue("a", Duration::from_secs(0), "boom2").unwrap();
        let second = q.take_batch(10).unwrap()[0].first_failed_at.unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn migrate_adds_first_failed_at_to_pre_existing_db() {
        // Simulate a queue created by an older build: events table WITHOUT
        // the first_failed_at column.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (
                   event_id TEXT PRIMARY KEY, event_type TEXT NOT NULL,
                   payload_json TEXT NOT NULL, occurred_at TEXT NOT NULL,
                   enqueued_at TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                   next_attempt_at TEXT, last_error TEXT
                 );",
            )
            .unwrap();
        }
        // Reopen through Queue::open — must migrate, then work end to end.
        let q = Queue::open(&path).unwrap();
        q.enqueue(&evt("migrated")).unwrap();
        q.requeue("migrated", Duration::from_secs(0), "x").unwrap();
        let batch = q.take_batch(10).unwrap();
        assert_eq!(batch.len(), 1);
        assert!(batch[0].first_failed_at.is_some());
        q.dead_letter("migrated", "parked").unwrap();
        assert_eq!(q.dead_letter_count().unwrap(), 1);
    }
}
