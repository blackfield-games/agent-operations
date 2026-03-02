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

/// Treat an `ALTER TABLE ... ADD COLUMN` that fails only because the column
/// already exists as a success — that is the expected case for a DB created on
/// a later boot. Any other error propagates unchanged.
fn ignore_duplicate_column(res: rusqlite::Result<usize>) -> Result<()> {
    match res {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("duplicate column name") => Ok(()),
        Err(e) => Err(e.into()),
    }
}

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
                 id         TEXT PRIMARY KEY,
                 spec_json  TEXT NOT NULL,
                 status     TEXT NOT NULL,
                 started_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS results (
                 job_id      TEXT NOT NULL,
                 result_json TEXT NOT NULL,
                 earner      TEXT NOT NULL,
                 created_at  INTEGER NOT NULL
             );
             -- Idempotency: at most one recorded result per job.
             CREATE UNIQUE INDEX IF NOT EXISTS idx_results_job_id ON results(job_id);",
        )?;
        // Migrate pre-existing DBs (created before `started_at` was added). The
        // column already exists on a later boot, so we swallow only that one
        // error and let any other failure propagate.
        ignore_duplicate_column(
            conn.execute("ALTER TABLE jobs ADD COLUMN started_at INTEGER", []),
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
                    "UPDATE jobs
                     SET status = ?1, started_at = CAST(strftime('%s','now') AS INTEGER)
                     WHERE id = ?2",
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
            "INSERT INTO jobs (id, spec_json, status, started_at) VALUES (?1, ?2, ?3, NULL)
             ON CONFLICT(id) DO UPDATE SET spec_json = ?2, status = ?3, started_at = NULL",
            (job.id.to_string(), spec_json, STATUS_QUEUED),
        )?;
        Ok(())
    }

    /// Record a validated result: insert into `results` and mark the job
    /// `done`. Both happen in one transaction.
    ///
    /// Idempotent: a duplicate result for the same job (e.g. a retried submit)
    /// is a no-op on the `results` table thanks to `ON CONFLICT DO NOTHING`, so
    /// `completed_count` is not double-counted. The job is still ensured `done`
    /// either way, so a re-record never errors.
    pub fn record_completed(&mut self, result: &JobResult) -> Result<()> {
        let result_json = serde_json::to_string(result)?;
        let job_id = result.job_id.to_string();
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let tx = self.conn.transaction()?;
        // ON CONFLICT(job_id): a second result for the same job is ignored so
        // we never double count. `changed` is 0 when the row already existed.
        let changed = tx.execute(
            "INSERT INTO results (job_id, result_json, earner, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(job_id) DO NOTHING",
            (
                &job_id,
                result_json,
                &result.earner_address,
                created_at,
            ),
        )?;
        if changed == 0 {
            tracing::warn!(%job_id, "duplicate result ignored (already recorded)");
        }
        // Mark done + clear started_at regardless, so a duplicate submit still
        // leaves the job in a consistent terminal state.
        tx.execute(
            "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2",
            (STATUS_DONE, &job_id),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Reclaim jobs orphaned `in_flight` by a crash: a job is marked in_flight
    /// when an earner takes it, but if the coordinator dies before the result
    /// comes back the job is stuck. Called once on startup to put them back on
    /// the queue. Returns the number of jobs reclaimed.
    pub fn recover_in_flight(&self) -> Result<usize> {
        let recovered = self.conn.execute(
            "UPDATE jobs SET status = ?1, started_at = NULL WHERE status = ?2",
            (STATUS_QUEUED, STATUS_IN_FLIGHT),
        )?;
        Ok(recovered)
    }

    /// Requeue in-flight jobs whose deadline has elapsed and return their ids.
    ///
    /// A job's deadline is `started_at + JobSpec::deadline_secs`. Any in-flight
    /// job at or past that point (relative to `now_secs`) is put back on the
    /// queue so another earner can pick it up. `now_secs` is passed in (epoch
    /// seconds) so callers — and tests — control the clock.
    pub fn reap_expired(&self, now_secs: i64) -> Result<Vec<uuid::Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_json, started_at FROM jobs
             WHERE status = ?1 AND started_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([STATUS_IN_FLIGHT], |row| {
            let id: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            let started_at: i64 = row.get(2)?;
            Ok((id, spec_json, started_at))
        })?;

        let mut expired = Vec::new();
        for row in rows {
            let (id, spec_json, started_at) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            if now_secs - started_at >= job.deadline_secs as i64 {
                self.conn.execute(
                    "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2",
                    (STATUS_QUEUED, &id),
                )?;
                expired.push(job.id);
            }
        }
        Ok(expired)
    }

    /// Current lifecycle status of a job, or `None` if the id is unknown.
    /// Lets the submit path gate results to jobs that are actually in_flight.
    pub fn job_status(&self, id: &uuid::Uuid) -> Result<Option<String>> {
        let status = self
            .conn
            .query_row(
                "SELECT status FROM jobs WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(status)
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

    /// Count of jobs currently in the `in_flight` state (for `/stats`).
    pub fn in_flight_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status = ?1",
            [STATUS_IN_FLIGHT],
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
