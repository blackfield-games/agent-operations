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
use proto::{JobKind, JobResult, JobSpec};
use rusqlite::Connection;
use std::collections::HashMap;

/// Job lifecycle in the `jobs` table.
const STATUS_QUEUED: &str = "queued";
const STATUS_IN_FLIGHT: &str = "in_flight";
const STATUS_DONE: &str = "done";
/// Terminal failure state: the job has been dispatched `max_attempts` times
/// without a successful result and will no longer be requeued.
const STATUS_FAILED: &str = "failed";

/// The full set of valid job lifecycle statuses, in lifecycle order. Single
/// source of truth for callers that validate an incoming status string (e.g.
/// the `GET /jobs?status=` filter validates against this before querying).
pub const JOB_STATUSES: [&str; 4] =
    [STATUS_QUEUED, STATUS_IN_FLIGHT, STATUS_DONE, STATUS_FAILED];

/// Outcome returned by `reap_expired`, split into jobs that were requeued for
/// another attempt and jobs that have been dead-lettered into `failed`.
#[derive(Debug, Default, PartialEq)]
pub struct ReapOutcome {
    /// Job ids moved back to `queued` (still have remaining attempts).
    pub requeued: Vec<uuid::Uuid>,
    /// Job ids moved to the terminal `failed` status (exhausted all attempts).
    pub failed: Vec<uuid::Uuid>,
}

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
                 started_at INTEGER,
                 attempts   INTEGER NOT NULL DEFAULT 0
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
        // Migrate pre-existing DBs (created before `attempts` was added). The
        // column already exists on a later boot, so we swallow only that one
        // error and let any other failure propagate.
        ignore_duplicate_column(
            conn.execute(
                "ALTER TABLE jobs ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0",
                [],
            ),
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
                     SET status     = ?1,
                         started_at = CAST(strftime('%s','now') AS INTEGER),
                         attempts   = attempts + 1
                     WHERE id = ?2",
                    (STATUS_IN_FLIGHT, &id),
                )?;
                return Ok(Some(job));
            }
        }
        Ok(None)
    }

    /// Put a job back on the queue (rejected submission / dropped connection),
    /// or dead-letter it if it has already been dispatched `max_attempts` times.
    ///
    /// Reads the job's current `attempts` from the table (treats it as 0 if the
    /// row does not exist yet, mirroring the `QueryReturnedNoRows` handling in
    /// `job_status`). If `attempts >= max_attempts` the job is moved to the
    /// terminal `failed` status; otherwise it is returned to `queued`. In both
    /// cases `started_at` is cleared. The latest `spec` is always upserted so
    /// this works even for jobs not yet in the table.
    ///
    /// Returns `true` iff the job was dead-lettered (i.e. moved to `failed`).
    pub fn requeue(&self, job: &JobSpec, max_attempts: u32) -> Result<bool> {
        // Read the current attempt count for this job (0 if not yet inserted).
        let attempts: u32 = self
            .conn
            .query_row(
                "SELECT attempts FROM jobs WHERE id = ?1",
                [job.id.to_string()],
                |row| row.get::<_, u32>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?
            .unwrap_or(0);

        let new_status = if attempts >= max_attempts { STATUS_FAILED } else { STATUS_QUEUED };
        let spec_json = serde_json::to_string(job)?;
        self.conn.execute(
            "INSERT INTO jobs (id, spec_json, status, started_at)
             VALUES (?1, ?2, ?3, NULL)
             ON CONFLICT(id) DO UPDATE SET spec_json = ?2, status = ?3, started_at = NULL",
            (job.id.to_string(), spec_json, new_status),
        )?;
        Ok(new_status == STATUS_FAILED)
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

    /// Requeue (or dead-letter) in-flight jobs whose deadline has elapsed.
    ///
    /// A job's deadline is `started_at + JobSpec::deadline_secs`. Any in-flight
    /// job at or past that point (relative to `now_secs`) is processed:
    ///
    /// * If its `attempts >= max_attempts` it is moved to the terminal `failed`
    ///   status and its id is pushed to `ReapOutcome::failed`.
    /// * Otherwise it is returned to `queued` and its id is pushed to
    ///   `ReapOutcome::requeued`.
    ///
    /// `now_secs` is passed in (epoch seconds) so callers — and tests — control
    /// the clock.
    pub fn reap_expired(&self, now_secs: i64, max_attempts: u32) -> Result<ReapOutcome> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_json, started_at, attempts FROM jobs
             WHERE status = ?1 AND started_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([STATUS_IN_FLIGHT], |row| {
            let id: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            let started_at: i64 = row.get(2)?;
            let attempts: u32 = row.get(3)?;
            Ok((id, spec_json, started_at, attempts))
        })?;

        let mut outcome = ReapOutcome::default();
        for row in rows {
            let (id, spec_json, started_at, attempts) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            if now_secs - started_at >= job.deadline_secs as i64 {
                if attempts >= max_attempts {
                    self.conn.execute(
                        "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2",
                        (STATUS_FAILED, &id),
                    )?;
                    outcome.failed.push(job.id);
                } else {
                    self.conn.execute(
                        "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2",
                        (STATUS_QUEUED, &id),
                    )?;
                    outcome.requeued.push(job.id);
                }
            }
        }
        Ok(outcome)
    }

    /// Bump an in-flight job's `started_at` to `now_secs` on an earner heartbeat,
    /// so the deadline reaper measures the deadline window from the last sign of
    /// life rather than from dispatch. A job that keeps heartbeating is making
    /// progress and won't be reaped; a silent earner still hits the deadline.
    /// No-op (returns `false`) for a job that is not currently `in_flight`
    /// (a stale/late heartbeat for an already-completed or requeued job).
    pub fn touch(&self, job_id: &uuid::Uuid, now_secs: i64) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE jobs SET started_at = ?1 WHERE id = ?2 AND status = ?3",
            (now_secs, job_id.to_string(), STATUS_IN_FLIGHT),
        )?;
        Ok(updated > 0)
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

    /// Most-recent jobs (rowid DESC) capped at `limit`, as `(id, kind, status)`
    /// triples for the `GET /jobs` listing. When `status` is `Some`, only jobs
    /// in that lifecycle status are returned; `None` returns all statuses.
    /// Empty when nothing matches.
    ///
    /// The `?1 IS NULL OR status = ?1` guard binds the optional filter as one
    /// nullable parameter: a NULL filter matches every row, a concrete status
    /// filters. Callers validate the status string (see `JOB_STATUSES`) before
    /// calling; either way it is bound as a parameter, never interpolated.
    pub fn list_jobs(
        &self,
        limit: usize,
        status: Option<&str>,
    ) -> Result<Vec<(uuid::Uuid, JobKind, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT spec_json, status FROM jobs
             WHERE (?1 IS NULL OR status = ?1)
             ORDER BY rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![status, limit as i64], |row| {
            let spec_json: String = row.get(0)?;
            let status: String = row.get(1)?;
            Ok((spec_json, status))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (spec_json, status) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            out.push((job.id, job.kind, status));
        }
        Ok(out)
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

    /// Count of queued jobs grouped by `JobKind` (for the `/stats` backlog
    /// composition). Decodes each queued spec's kind in Rust, mirroring the
    /// decode in `take_next`/`reap_expired`. Empty map when nothing is queued.
    pub fn queued_count_by_kind(&self) -> Result<HashMap<JobKind, usize>> {
        let mut stmt = self
            .conn
            .prepare("SELECT spec_json FROM jobs WHERE status = ?1")?;
        let rows = stmt.query_map([STATUS_QUEUED], |row| row.get::<_, String>(0))?;
        let mut counts: HashMap<JobKind, usize> = HashMap::new();
        for row in rows {
            let job: JobSpec = serde_json::from_str(&row?)?;
            *counts.entry(job.kind).or_insert(0) += 1;
        }
        Ok(counts)
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

    /// Count of jobs in the terminal `failed` status (dead-lettered after
    /// exhausting `max_attempts` dispatches). Exposed via `/stats`.
    pub fn failed_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE status = ?1",
            [STATUS_FAILED],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }
}
