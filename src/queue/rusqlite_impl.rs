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

/// One event sitting in the queue.
#[derive(Debug, Clone)]
pub struct QueuedEvent {
    pub event_id: String,
    pub event: NormalizedEvent,
    pub attempts: u32,
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
        Ok(Self { conn })
    }

    /// Open an in-memory queue. Useful for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory queue db")?;
        conn.execute_batch(SCHEMA_SQL)
            .context("bootstrapping queue schema")?;
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
                "SELECT event_id, payload_json, attempts
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
                Ok((event_id, payload_json, attempts))
            })
            .context("executing take_batch")?;

        let mut out = Vec::new();
        for row in rows {
            let (event_id, payload_json, attempts) = row.context("reading event row")?;
            let event: NormalizedEvent =
                serde_json::from_str(&payload_json).context("deserializing event payload")?;
            out.push(QueuedEvent {
                event_id,
                event,
                attempts: attempts.max(0) as u32,
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
    /// exponential-backoff policy in `sync::batching`.
    pub fn requeue(&self, event_id: &str, delay: Duration, error: &str) -> Result<()> {
        let next: DateTime<Utc> = Utc::now()
            + chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::seconds(60));
        self.conn
            .execute(
                "UPDATE events
                 SET attempts = attempts + 1,
                     next_attempt_at = ?1,
                     last_error = ?2
                 WHERE event_id = ?3",
                params![next.to_rfc3339(), error, event_id],
            )
            .context("requeue")?;
        Ok(())
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
