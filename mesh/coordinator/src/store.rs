//! SQLite-backed persistence for the coordinator's job queue and completed
//! results, so both survive a process restart.
//!
//! State that must outlive a restart lives here: the job queue (with each
//! job's lifecycle status) and the validated results. Earner registrations
//! stay in-memory (they are ephemeral — an earner re-Hellos on reconnect).
//!
//! Storage shape: `JobSpec`/`JobResult` are serialized to JSON and stored as
//! text, keeping the wire types untouched. We use the `rusqlite` "bundled"
//! feature so no system SQLite is required.

use anyhow::Result;
use proto::{JobResult, JobSpec};
use rusqlite::Connection;

/// Job lifecycle in the `jobs` table.
const STATUS_QUEUED: &str = "queued";
const STATUS_IN_FLIGHT: &str = "in_flight";
const STATUS_DONE: &str = "done";

/// SQLite-backed store. Wraps a single connection; callers serialize access
/// (the coordinator holds it behind a `Mutex`). At this scale a single
/// connection is correct and simple.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) a file-backed DB at `path` and ensure the
    /// schema exists.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an in-memory DB (tests). The DB lives only as long as the `Store`.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                 id        TEXT PRIMARY KEY,
                 spec_json TEXT NOT NULL,
                 status    TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS results (
                 job_id      TEXT NOT NULL,
                 result_json TEXT NOT NULL,
                 earner      TEXT NOT NULL,
                 created_at  INTEGER NOT NULL
             );",
        )?;
        Ok(Self { conn })
    }

    /// True when no jobs exist yet — used to decide whether to seed a fresh DB
    /// without double-seeding an existing one across restarts.
    pub fn jobs_empty(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))?;
        Ok(count == 0)
    }

    /// Insert (or upsert by id) a job in the `queued` state.
    pub fn enqueue(&self, job: &JobSpec) -> Result<()> {
        let spec_json = serde_json::to_string(job)?;
        self.conn.execute(
            "INSERT INTO jobs (id, spec_json, status) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET spec_json = ?2, status = ?3",
            (job.id.to_string(), spec_json, STATUS_QUEUED),
        )?;
        Ok(())
    }

    /// Pop the most recently inserted queued job whose kind passes `accept`,
    /// marking it `in_flight`. Returns `None` if no queued job matches.
    ///
    /// "Most recent" mirrors the prior `Vec::pop` / `rposition` behavior:
    /// rowid is monotonic on insert, so highest rowid == most recent.
    pub fn take_next<F>(&self, accept: F) -> Result<Option<JobSpec>>
    where
        F: Fn(&JobSpec) -> bool,
    {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_json FROM jobs WHERE status = ?1 ORDER BY rowid DESC",
        )?;
        let rows = stmt.query_map([STATUS_QUEUED], |row| {
            let id: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            Ok((id, spec_json))
        })?;

        for row in rows {
            let (id, spec_json) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            if accept(&job) {
                self.conn.execute(
                    "UPDATE jobs SET status = ?1 WHERE id = ?2",
                    (STATUS_IN_FLIGHT, &id),
                )?;
                return Ok(Some(job));
            }
        }
        Ok(None)
    }

    /// Put a job back on the queue (rejected submission / dropped connection).
    /// Stores the latest spec so requeue works even for jobs not currently in
    /// the table.
    pub fn requeue(&self, job: &JobSpec) -> Result<()> {
        let spec_json = serde_json::to_string(job)?;
        self.conn.execute(
            "INSERT INTO jobs (id, spec_json, status) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET spec_json = ?2, status = ?3",
            (job.id.to_string(), spec_json, STATUS_QUEUED),
        )?;
        Ok(())
    }

    /// Record a validated result: insert into `results` and mark the job
    /// `done`. Both happen in one transaction.
    pub fn record_completed(&mut self, result: &JobResult) -> Result<()> {
        let result_json = serde_json::to_string(result)?;
        let job_id = result.job_id.to_string();
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO results (job_id, result_json, earner, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            (
                &job_id,
                result_json,
                &result.earner_address,
                created_at,
            ),
        )?;
        tx.execute(
            "UPDATE jobs SET status = ?1 WHERE id = ?2",
            (STATUS_DONE, &job_id),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Count of jobs currently in the `queued` state (for `/stats`).
    pub fn queued_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status = ?1",
            [STATUS_QUEUED],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Count of recorded results (for `/stats`).
    pub fn completed_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))?;
        Ok(count as usize)
    }
}
