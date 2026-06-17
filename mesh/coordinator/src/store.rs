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
                 id           TEXT PRIMARY KEY,
                 spec_json    TEXT NOT NULL,
                 status       TEXT NOT NULL,
                 started_at   INTEGER,
                 attempts     INTEGER NOT NULL DEFAULT 0,
                 dispatch_seq INTEGER NOT NULL DEFAULT 0
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
        // Migrate pre-existing DBs (created before `dispatch_seq` was added). The
        // per-dispatch fence defaults to 0; the first `take_next` after a restart
        // bumps it to 1, so a migrated in-flight job that is recovered and
        // re-dispatched gets a fresh seq. Swallow only the duplicate-column error.
        ignore_duplicate_column(
            conn.execute(
                "ALTER TABLE jobs ADD COLUMN dispatch_seq INTEGER NOT NULL DEFAULT 0",
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
    /// marking it `in_flight` and stamping a fresh per-dispatch fence. Returns
    /// the job with its new `dispatch_seq`, or `None` if no queued job matches.
    ///
    /// `dispatch_seq` is a monotonic per-job counter bumped on every dispatch. It
    /// identifies *this* hand-out: the coordinator remembers it for the session
    /// and later requires it to match (`current_dispatch_seq`) before settling,
    /// requeueing, or sliding the deadline — so a job reaped and reassigned to a
    /// new earner can neither be settled nor preempted by the previous holder.
    ///
    /// "Most recent" mirrors the prior `Vec::pop` / `rposition` behavior:
    /// rowid is monotonic on insert, so highest rowid == most recent.
    pub fn take_next<F>(&self, accept: F) -> Result<Option<(JobSpec, i64)>>
    where
        F: Fn(&JobSpec) -> bool,
    {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_json, dispatch_seq FROM jobs WHERE status = ?1 ORDER BY rowid DESC",
        )?;
        let rows = stmt.query_map([STATUS_QUEUED], |row| {
            let id: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            let dispatch_seq: i64 = row.get(2)?;
            Ok((id, spec_json, dispatch_seq))
        })?;

        for row in rows {
            let (id, spec_json, dispatch_seq) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            if accept(&job) {
                let new_seq = dispatch_seq + 1;
                self.conn.execute(
                    "UPDATE jobs
                     SET status       = ?1,
                         started_at   = CAST(strftime('%s','now') AS INTEGER),
                         attempts     = attempts + 1,
                         dispatch_seq = ?2
                     WHERE id = ?3",
                    (STATUS_IN_FLIGHT, new_seq, &id),
                )?;
                return Ok(Some((job, new_seq)));
            }
        }
        Ok(None)
    }

    /// Put a job back on the queue (rejected submission / dropped connection),
    /// or dead-letter it if it has already been dispatched `max_attempts` times —
    /// but ONLY while it is still `in_flight`.
    ///
    /// This is an earner-driven action (its offer was rejected or its socket
    /// dropped). If the reaper has already requeued or dead-lettered the job, the
    /// late reject/disconnect is a no-op, so it can't resurrect a terminal job or
    /// re-queue one the reaper just parked. Reads `attempts` to decide:
    /// `>= max_attempts` → terminal `failed`, otherwise back to `queued`; clears
    /// `started_at`. Reaper-driven expiry lives in `reap_expired`.
    ///
    /// Residual race (tracked as `mesh-dispatch-lease-fence`): the in_flight
    /// check cannot distinguish "in_flight under me" from "in_flight under a
    /// later earner". If a slow earner's job is reaped, reassigned to a new
    /// earner, and only then the slow earner disconnects, this requeue still sees
    /// `in_flight` and preempts the new holder. Closing it needs a per-dispatch
    /// fence token, which the wire protocol does not yet carry.
    ///
    /// Returns `true` iff the job was dead-lettered (moved to `failed`); `false`
    /// if it was requeued OR if it was a no-op (not in_flight / unknown).
    pub fn requeue(&self, job: &JobSpec, max_attempts: u32) -> Result<bool> {
        // Act only on a still-in_flight job; read its status + attempts together.
        let row: Option<(String, u32)> = self
            .conn
            .query_row(
                "SELECT status, attempts FROM jobs WHERE id = ?1",
                [job.id.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some((status, attempts)) = row else { return Ok(false) }; // unknown job
        if status != STATUS_IN_FLIGHT {
            return Ok(false); // already reaped or terminal — don't clobber
        }

        let new_status = if attempts >= max_attempts { STATUS_FAILED } else { STATUS_QUEUED };
        // Guard the transition on in_flight too, so it stays atomic with the read.
        self.conn.execute(
            "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2 AND status = ?3",
            (new_status, job.id.to_string(), STATUS_IN_FLIGHT),
        )?;
        Ok(new_status == STATUS_FAILED)
    }

    /// Settle a job with its validated result: insert into `results` and mark
    /// the job `done`, in one transaction — but ONLY while the job is currently
    /// `in_flight`. Returns `true` if it settled, `false` if the job was not
    /// in_flight and nothing was written.
    ///
    /// The in_flight guard is the data-layer backstop against a stale or
    /// replayed submit: a job that has been reaped, reassigned, already settled,
    /// or never existed cannot be resurrected to `done` or credited twice — even
    /// if a caller forgets to pre-check lifecycle. The `UNIQUE(job_id)` index on
    /// `results` is a further guard so a result is recorded at most once.
    pub fn record_completed(&mut self, result: &JobResult) -> Result<bool> {
        let job_id = result.job_id.to_string();
        let tx = self.conn.transaction()?;

        // Re-check lifecycle inside the transaction; only an in_flight job is
        // settle-able. A non-in_flight (queued/done/failed) or unknown job is
        // refused, leaving its state untouched.
        let status: Option<String> = tx
            .query_row("SELECT status FROM jobs WHERE id = ?1", [&job_id], |row| {
                row.get::<_, String>(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        if status.as_deref() != Some(STATUS_IN_FLIGHT) {
            return Ok(false);
        }

        let result_json = serde_json::to_string(result)?;
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // ON CONFLICT(job_id) DO NOTHING is belt-and-suspenders: the in_flight
        // guard already blocks a second settle, but the unique index keeps a
        // duplicate insert a no-op rather than an error.
        tx.execute(
            "INSERT INTO results (job_id, result_json, earner, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(job_id) DO NOTHING",
            (&job_id, result_json, &result.earner_address, created_at),
        )?;
        tx.execute(
            "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2",
            (STATUS_DONE, &job_id),
        )?;
        tx.commit()?;
        Ok(true)
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

    /// Current `dispatch_seq` of a job, or `None` if the id is unknown. The
    /// coordinator compares the seq it dispatched against this — under the same
    /// store lock that guards the settle/requeue/touch mutation — so only the
    /// holder of the *current* dispatch can act on the job. A job reaped and
    /// reassigned to a new earner carries a higher seq, which fences out the
    /// previous holder's late submit, disconnect-requeue, or stale heartbeat.
    pub fn current_dispatch_seq(&self, id: &uuid::Uuid) -> Result<Option<i64>> {
        let seq = self
            .conn
            .query_row(
                "SELECT dispatch_seq FROM jobs WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(seq)
    }

    /// Full `JobSpec` for a job id, or `None` if the id is unknown. Decodes the
    /// stored `spec_json`. Backs the `GET /jobs/{id}` full-detail endpoint
    /// (vs the status-only `job_status`).
    pub fn get_job(&self, id: &uuid::Uuid) -> Result<Option<JobSpec>> {
        let spec_json: Option<String> = self
            .conn
            .query_row(
                "SELECT spec_json FROM jobs WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        match spec_json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    /// The recorded `JobResult` for a job id, or `None` when the job has no
    /// result yet (it has not completed). Decodes the stored `result_json`.
    /// Backs the `GET /jobs/{id}` full-detail endpoint.
    pub fn get_result(&self, id: &uuid::Uuid) -> Result<Option<JobResult>> {
        let result_json: Option<String> = self
            .conn
            .query_row(
                "SELECT result_json FROM results WHERE job_id = ?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        match result_json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
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

    /// Count of jobs in `status` grouped by `JobKind` (for the `/stats`
    /// composition fields). Decodes each matching spec's kind in Rust, mirroring
    /// the decode in `take_next`/`reap_expired`. Empty map when nothing matches.
    fn count_by_kind(&self, status: &str) -> Result<HashMap<JobKind, usize>> {
        let mut stmt = self
            .conn
            .prepare("SELECT spec_json FROM jobs WHERE status = ?1")?;
        let rows = stmt.query_map([status], |row| row.get::<_, String>(0))?;
        let mut counts: HashMap<JobKind, usize> = HashMap::new();
        for row in rows {
            let job: JobSpec = serde_json::from_str(&row?)?;
            *counts.entry(job.kind).or_insert(0) += 1;
        }
        Ok(counts)
    }

    /// Count of QUEUED jobs grouped by `JobKind` (the `/stats` backlog
    /// composition). Empty map when nothing is queued.
    pub fn queued_count_by_kind(&self) -> Result<HashMap<JobKind, usize>> {
        self.count_by_kind(STATUS_QUEUED)
    }

    /// Count of IN-FLIGHT jobs grouped by `JobKind` (the `/stats` in-flight
    /// composition). Empty map when nothing is in flight.
    pub fn in_flight_count_by_kind(&self) -> Result<HashMap<JobKind, usize>> {
        self.count_by_kind(STATUS_IN_FLIGHT)
    }

    /// Count of DONE jobs grouped by `JobKind` (the `/stats` completed
    /// composition). A job reaches `done` only via `record_completed`, which
    /// also inserts its single result, so this sums to `completed_count`. Empty
    /// map when nothing has completed.
    pub fn done_count_by_kind(&self) -> Result<HashMap<JobKind, usize>> {
        self.count_by_kind(STATUS_DONE)
    }

    /// Count of FAILED (dead-lettered) jobs grouped by `JobKind` (the `/stats`
    /// failed composition). A job reaches `failed` only after exhausting
    /// `max_attempts` dispatches, so this sums to `failed_count`. Empty map when
    /// nothing has failed.
    pub fn failed_count_by_kind(&self) -> Result<HashMap<JobKind, usize>> {
        self.count_by_kind(STATUS_FAILED)
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

    /// Epoch-seconds `started_at` of the OLDEST in-flight job (the minimum over
    /// every `in_flight` job), or `None` when nothing is in flight. The `/stats`
    /// handler turns this into an `oldest_in_flight_secs` age (`now - this`) so
    /// the HUD can surface the longest-running dispatch — a queue-health signal
    /// that needs no schema migration: `started_at` is set on `take_next`,
    /// bumped by heartbeats (`touch`), and cleared on completion/requeue/recover.
    pub fn oldest_in_flight_started_at(&self) -> Result<Option<i64>> {
        // SQL MIN over zero matching rows is NULL → None. (in_flight rows always
        // have started_at set; the IS NOT NULL guard is belt-and-suspenders.)
        let oldest: Option<i64> = self.conn.query_row(
            "SELECT MIN(started_at) FROM jobs WHERE status = ?1 AND started_at IS NOT NULL",
            [STATUS_IN_FLIGHT],
            |row| row.get(0),
        )?;
        Ok(oldest)
    }

    /// Count of jobs dispatched more than once (`attempts > 1`): `take_next`
    /// handed them out, the reaper requeued them on a missed deadline, and they
    /// were dispatched again. A cumulative reaper-churn signal for `/stats` —
    /// counts each such job once regardless of current status, since `attempts`
    /// is bumped on every dispatch and never reset. Zero on a healthy mesh where
    /// every job lands on its first dispatch.
    pub fn redispatched_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM jobs WHERE attempts > 1", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Count of recorded results (for `/stats`).
    pub fn completed_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Sum of `render_seconds` across all recorded results — the mesh-output
    /// metric surfaced at `/stats` ("N render-seconds produced"). Decodes each
    /// stored `JobResult` in Rust and sums its `render_seconds`, mirroring the
    /// per-kind decode pattern. Widened to `u64` so a large mesh-wide total
    /// can't overflow the `u32` per-result field. Zero when nothing has
    /// completed.
    pub fn total_render_seconds(&self) -> Result<u64> {
        let mut stmt = self.conn.prepare("SELECT result_json FROM results")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut total: u64 = 0;
        for row in rows {
            let result: JobResult = serde_json::from_str(&row?)?;
            total += result.render_seconds as u64;
        }
        Ok(total)
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

    /// Completed-result count grouped by earner address (`results.earner`), for
    /// the `GET /earners` leaderboard. Counts recorded results per earner in SQL.
    /// Empty map when nothing has completed.
    pub fn completed_count_by_earner(&self) -> Result<HashMap<String, usize>> {
        let mut stmt = self
            .conn
            .prepare("SELECT earner, COUNT(*) FROM results GROUP BY earner")?;
        let rows = stmt.query_map([], |row| {
            let earner: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((earner, count as usize))
        })?;
        let mut counts = HashMap::new();
        for row in rows {
            let (earner, count) = row?;
            counts.insert(earner, count);
        }
        Ok(counts)
    }

    /// Sum of `render_seconds` grouped by earner address, for the `GET /earners`
    /// leaderboard. Decodes each recorded `JobResult` and accumulates its
    /// `render_seconds` (widened to `u64`, mirroring `total_render_seconds`)
    /// under its `results.earner` key. Empty map when nothing has completed.
    pub fn render_seconds_by_earner(&self) -> Result<HashMap<String, u64>> {
        let mut stmt = self.conn.prepare("SELECT earner, result_json FROM results")?;
        let rows = stmt.query_map([], |row| {
            let earner: String = row.get(0)?;
            let result_json: String = row.get(1)?;
            Ok((earner, result_json))
        })?;
        let mut totals: HashMap<String, u64> = HashMap::new();
        for row in rows {
            let (earner, result_json) = row?;
            let result: JobResult = serde_json::from_str(&result_json)?;
            *totals.entry(earner).or_insert(0) += result.render_seconds as u64;
        }
        Ok(totals)
    }

    /// Sum of `max_payout_wei` across all DONE jobs — the HUD "total $BLCKFLD
    /// payable" metric. `max_payout_wei` is a 1e18-scale decimal string on the
    /// `JobSpec`, so each done job's spec is decoded and its value parsed as
    /// `u128` (wei; u128 max ≈ 3.4e38 ≈ 3.4e20 ether — ample headroom). Summed
    /// with `checked_add` so an implausible overflow errors rather than wraps.
    /// Zero when nothing has completed.
    pub fn total_payout_wei(&self) -> Result<u128> {
        let mut stmt = self.conn.prepare("SELECT spec_json FROM jobs WHERE status = ?1")?;
        let rows = stmt.query_map([STATUS_DONE], |row| row.get::<_, String>(0))?;
        let mut total: u128 = 0;
        for row in rows {
            let job: JobSpec = serde_json::from_str(&row?)?;
            let wei: u128 = job
                .max_payout_wei
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid max_payout_wei {:?}: {e}", job.max_payout_wei))?;
            total = total
                .checked_add(wei)
                .ok_or_else(|| anyhow::anyhow!("total_payout_wei overflowed u128"))?;
        }
        Ok(total)
    }

    /// Sum of `max_payout_wei` grouped by earner address — the per-earner
    /// counterpart to `total_payout_wei`, for the `GET /earners` leaderboard
    /// economics column. Joins each recorded result to its job (every recorded
    /// result's job is `done`, so the inner join is exact), decodes the
    /// `JobSpec`, and accumulates `max_payout_wei.parse::<u128>()` under the
    /// `results.earner` key with `checked_add` (an implausible overflow errors
    /// rather than wraps, mirroring `total_payout_wei`). Empty map when nothing
    /// has completed.
    pub fn payout_wei_by_earner(&self) -> Result<HashMap<String, u128>> {
        let mut stmt = self
            .conn
            .prepare("SELECT r.earner, j.spec_json FROM results r JOIN jobs j ON r.job_id = j.id")?;
        let rows = stmt.query_map([], |row| {
            let earner: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            Ok((earner, spec_json))
        })?;
        let mut totals: HashMap<String, u128> = HashMap::new();
        for row in rows {
            let (earner, spec_json) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            let wei: u128 = job
                .max_payout_wei
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid max_payout_wei {:?}: {e}", job.max_payout_wei))?;
            let slot = totals.entry(earner).or_insert(0u128);
            *slot = slot
                .checked_add(wei)
                .ok_or_else(|| anyhow::anyhow!("payout_wei_by_earner overflowed u128"))?;
        }
        Ok(totals)
    }
}
