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
use std::collections::{HashMap, HashSet};

/// Job lifecycle in the `jobs` table.
const STATUS_QUEUED: &str = "queued";
const STATUS_IN_FLIGHT: &str = "in_flight";
const STATUS_DONE: &str = "done";
/// Terminal failure state: the job exhausted its dispatch `attempts` /
/// earner-`faults` budget, or outlived its absolute wall-clock TTL
/// (`reap_ttl_expired`), and will no longer be requeued.
const STATUS_FAILED: &str = "failed";

/// The full set of valid job lifecycle statuses, in lifecycle order. Single
/// source of truth for callers that validate an incoming status string (e.g.
/// the `GET /jobs?status=` filter validates against this before querying).
pub const JOB_STATUSES: [&str; 4] = [STATUS_QUEUED, STATUS_IN_FLIGHT, STATUS_DONE, STATUS_FAILED];

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

/// True if `table` currently has a column named `column`. Used to detect a legacy
/// table shape before a one-time rebuild migration. `table` is always a hardcoded
/// literal here (never caller input), so the unparameterizable PRAGMA name is safe.
fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        (table, column),
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// SQLite-backed store. Wraps a single connection; callers serialize access
/// (the coordinator holds it behind a `Mutex`). At this scale a single
/// connection is correct and simple.
pub struct Store {
    conn: Connection,
    /// Wei charged per render-second to a job's buyer at settle, recorded as a
    /// pending ComputeMeter debit. `0` (the default) disables metering: no debit
    /// row is written. Set from the `--compute-rate-wei` operator knob; read by
    /// [`Store::record_completed`].
    compute_rate_wei: u128,
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

    /// Set the per-render-second wei rate charged to a job's buyer at settle (a
    /// pending ComputeMeter debit). `0` (the default) disables metering. Consuming
    /// builder so the rate is fixed at open from the `--compute-rate-wei` knob.
    pub fn with_compute_rate_wei(mut self, rate_wei: u128) -> Self {
        self.compute_rate_wei = rate_wei;
        self
    }

    /// Test-only: does an index named `name` exist (i.e. did schema init create it)?
    #[cfg(test)]
    pub fn has_index(&self, name: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Test-only: the concatenated EXPLAIN QUERY PLAN `detail` rows for the TTL
    /// reaper's exact predicate, so a test can assert the planner actually picks the
    /// index (creation alone doesn't imply use — FM2). Mirrors `reap_ttl_expired`.
    #[cfg(test)]
    pub fn reap_ttl_query_plan(&self) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "EXPLAIN QUERY PLAN
             SELECT id, spec_json, created_at FROM jobs
             WHERE status IN (?1, ?2) AND created_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([STATUS_QUEUED, STATUS_IN_FLIGHT], |r| {
            r.get::<_, String>(3) // EXPLAIN QUERY PLAN columns: id,parent,notused,detail
        })?;
        let mut plan = String::new();
        for row in rows {
            plan.push_str(&row?);
            plan.push('\n');
        }
        Ok(plan)
    }

    /// Test-only: the concatenated EXPLAIN QUERY PLAN `detail` rows for the dispatch
    /// SELECT in `take_next_inner`, so a test can assert the planner serves the
    /// oldest-first ordering from `idx_jobs_status_created_at` with no temp-b-tree
    /// sort. Mirrors the live query verbatim.
    #[cfg(test)]
    pub fn dispatch_query_plan(&self) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "EXPLAIN QUERY PLAN
             SELECT id, spec_json, dispatch_seq FROM jobs WHERE status = ?1
             ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map([STATUS_QUEUED], |r| {
            r.get::<_, String>(3) // EXPLAIN QUERY PLAN columns: id,parent,notused,detail
        })?;
        let mut plan = String::new();
        for row in rows {
            plan.push_str(&row?);
            plan.push('\n');
        }
        Ok(plan)
    }

    /// Test-only: the EXPLAIN QUERY PLAN for the ACTUAL `enqueue_within_cap`
    /// statement (the full `INSERT ... SELECT ... WHERE (SELECT COUNT(*) ...) < ?`,
    /// byte-for-byte), so a test can assert the gating COUNT subquery is served by
    /// `idx_jobs_status_created_at` and never full-scans the (unbounded, terminal-
    /// row-bearing) jobs table — the FM1 that would let a flood turn each cap check
    /// into an O(table) scan. Explaining the real statement (not a standalone proxy
    /// for the subquery) closes any planner divergence between the two forms. The
    /// bind values are placeholders: EXPLAIN plans the statement, it does not insert.
    #[cfg(test)]
    pub fn enqueue_within_cap_query_plan(&self, max_queued: usize) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "EXPLAIN QUERY PLAN
             INSERT INTO jobs (id, spec_json, status, created_at)
             SELECT ?1, ?2, ?3, CAST(strftime('%s','now') AS INTEGER)
             WHERE (SELECT COUNT(*) FROM jobs WHERE status = ?3) < ?4",
        )?;
        let rows = stmt.query_map(
            ("explain-only", "{}", STATUS_QUEUED, max_queued as i64),
            |r| r.get::<_, String>(3),
        )?;
        let mut plan = String::new();
        for row in rows {
            plan.push_str(&row?);
            plan.push('\n');
        }
        Ok(plan)
    }

    /// Test-only: the EXPLAIN QUERY PLAN for the ACTUAL `attempt_fault_totals`
    /// statement (the `SUM(attempts), SUM(faults) FROM jobs`, byte-for-byte), so a
    /// test can assert the planner serves it from the `idx_jobs_attempts_faults`
    /// COVERING index — scanning the skinny (attempts, faults) entries — instead of
    /// a full `SCAN jobs` that drags every row's inline `spec_json` through the
    /// b-tree (FM1: creation alone doesn't imply use). Mirrors the live query.
    #[cfg(test)]
    pub fn attempt_fault_totals_query_plan(&self) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "EXPLAIN QUERY PLAN
             SELECT COALESCE(SUM(attempts), 0), COALESCE(SUM(faults), 0) FROM jobs",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(3))?;
        let mut plan = String::new();
        for row in rows {
            plan.push_str(&row?);
            plan.push('\n');
        }
        Ok(plan)
    }

    /// Test-only: the EXPLAIN QUERY PLAN for the ACTUAL `faults_by_earner` statement
    /// (the `SELECT earner, fault_count FROM earner_faults`, byte-for-byte), so a
    /// test can assert the per-earner counter read is a plain scan with NO `USE TEMP
    /// B-TREE FOR GROUP BY` — the counter collapses the old GROUP BY entirely.
    /// Mirrors the live query.
    #[cfg(test)]
    pub fn faults_by_earner_query_plan(&self) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "EXPLAIN QUERY PLAN
             SELECT earner, fault_count FROM earner_faults",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(3))?;
        let mut plan = String::new();
        for row in rows {
            plan.push_str(&row?);
            plan.push('\n');
        }
        Ok(plan)
    }

    /// Test-only: the EXPLAIN QUERY PLAN for the ACTUAL `prune_terminal_jobs`
    /// candidate SELECT (byte-for-byte), so a test can assert the aged-terminal scan
    /// is served by `idx_jobs_status_created_at` as a range over only the terminal
    /// rows past the cutoff — never a full `SCAN jobs` over the very history the
    /// retention sweep exists to bound (FM1: creation alone doesn't imply use), and
    /// with no `USE TEMP B-TREE` (the `LIMIT batch` needs no global sort). The bind
    /// values are placeholders: EXPLAIN plans the statement, it does not delete.
    #[cfg(test)]
    pub fn prune_terminal_query_plan(&self) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "EXPLAIN QUERY PLAN
             SELECT id FROM jobs
             WHERE status IN (?1, ?2)
               AND created_at IS NOT NULL
               AND created_at <= ?3
               AND id NOT IN (SELECT job_id FROM pending_attestations WHERE uid IS NULL)
               AND id NOT IN (SELECT job_id FROM pending_debits WHERE tx_hash IS NULL)
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![STATUS_DONE, STATUS_FAILED, 0i64, 1i64],
            |r| r.get::<_, String>(3),
        )?;
        let mut plan = String::new();
        for row in rows {
            plan.push_str(&row?);
            plan.push('\n');
        }
        Ok(plan)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                 id           TEXT PRIMARY KEY,
                 spec_json    TEXT NOT NULL,
                 status       TEXT NOT NULL,
                 started_at   INTEGER,
                 attempts     INTEGER NOT NULL DEFAULT 0,
                 faults       INTEGER NOT NULL DEFAULT 0,
                 dispatch_seq INTEGER NOT NULL DEFAULT 0,
                 created_at   INTEGER,
                 dispatched_to TEXT,
                 buyer        TEXT
             );
             CREATE TABLE IF NOT EXISTS results (
                 job_id      TEXT NOT NULL,
                 result_json TEXT NOT NULL,
                 earner      TEXT NOT NULL,
                 created_at  INTEGER NOT NULL
             );
             -- Idempotency: at most one recorded result per job.
             CREATE UNIQUE INDEX IF NOT EXISTS idx_results_job_id ON results(job_id);
             -- Per-earner GENUINE quality-fault tally (bad/forged/replayed
             -- signature, malformed/implausible content, submit-before-accept,
             -- job_id mismatch), for reputation attribution on /earners. ONE row
             -- per earner: `fault_count` is its lifetime count of fault EVENTS (no
             -- dedup — the same earner faulting the same job across reconnects
             -- counts twice). An honest Decline of an unsupported kind is NEVER
             -- tallied here (protocol-correct, not a fault). Distinct from the
             -- per-JOB `jobs.faults` budget (which a Decline DOES bump, so a job
             -- no live earner can serve still dead-letters): that counter has no
             -- earner identity, which is exactly why this table exists. A counter
             -- keyed by earner (not one row per fault) so it stays bounded by the
             -- distinct faulting-earner count, never by total faults ever recorded.
             CREATE TABLE IF NOT EXISTS earner_faults (
                 earner      TEXT PRIMARY KEY,
                 fault_count INTEGER NOT NULL DEFAULT 0
             );
             -- Pending EAS render receipts: written atomically with the settle
             -- (see record_completed) so a crash before the on-chain
             -- RenderReceipts.issueReceipt cannot lose a validated job's receipt.
             -- Fields mirror the contract's registered schema (see eas.rs). A row
             -- is PENDING while `uid IS NULL`; the relayer flips it by writing the
             -- returned attestation `uid` + `submitted_at` once issueReceipt lands.
             -- `job_id` is NOT NULL (SQLite would otherwise permit a NULL in a TEXT
             -- PRIMARY KEY): the retention sweep gates on `id NOT IN (SELECT job_id
             -- ... WHERE uid IS NULL)`, and a single NULL in that subquery makes the
             -- NOT IN evaluate NULL for every candidate, silently stalling pruning —
             -- so the column the gate reads must never be NULL.
             CREATE TABLE IF NOT EXISTS pending_attestations (
                 job_id         TEXT PRIMARY KEY NOT NULL,
                 earner         TEXT NOT NULL,
                 job_id_b32     TEXT NOT NULL,
                 render_seconds INTEGER NOT NULL,
                 job_kind       INTEGER NOT NULL,
                 output_hash    TEXT NOT NULL,
                 region_id_b32  TEXT NOT NULL,
                 created_at     INTEGER NOT NULL,
                 uid            TEXT,
                 submitted_at   INTEGER
             );
             -- Pending ComputeMeter debits: written atomically with the settle
             -- (see record_completed), the metering twin of pending_attestations,
             -- so a crash before the on-chain ComputeMeter.spend cannot lose a
             -- buyer's charge. Fields map to spend(buyer, amount, jobId) — see
             -- meter.rs. amount_wei is a decimal wei string (like max_payout_wei),
             -- never an INTEGER (a 1e18-scale value overflows i64). A row is
             -- PENDING while `tx_hash IS NULL`; the (operator-gated) relayer flips
             -- it by writing the spend tx_hash + submitted_at.
             -- `job_id` NOT NULL for the same reason as pending_attestations above:
             -- the retention `NOT IN (... WHERE tx_hash IS NULL)` gate must read a
             -- NULL-free column or a NULL would stall every prune.
             CREATE TABLE IF NOT EXISTS pending_debits (
                 job_id       TEXT PRIMARY KEY NOT NULL,
                 buyer        TEXT NOT NULL,
                 amount_wei   TEXT NOT NULL,
                 job_id_b32   TEXT NOT NULL,
                 created_at   INTEGER NOT NULL,
                 submitted_at INTEGER,
                 tx_hash      TEXT
             );",
        )?;
        // Migrate pre-existing DBs (created before the relayer's `uid` /
        // `submitted_at` columns). NULL on every existing row means "still
        // pending", which is correct: nothing had been relayed yet. Swallow only
        // the duplicate-column error.
        ignore_duplicate_column(
            conn.execute("ALTER TABLE pending_attestations ADD COLUMN uid TEXT", []),
        )?;
        ignore_duplicate_column(conn.execute(
            "ALTER TABLE pending_attestations ADD COLUMN submitted_at INTEGER",
            [],
        ))?;
        // Migrate pre-existing DBs (created before the debit relayer's `submitted_at`
        // / `tx_hash` columns). NULL on every existing row means "still pending",
        // which is correct: nothing had been spent on-chain yet. Mirrors the
        // pending_attestations migration above. Swallow only the duplicate-column error.
        ignore_duplicate_column(conn.execute(
            "ALTER TABLE pending_debits ADD COLUMN submitted_at INTEGER",
            [],
        ))?;
        ignore_duplicate_column(
            conn.execute("ALTER TABLE pending_debits ADD COLUMN tx_hash TEXT", []),
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
        ignore_duplicate_column(conn.execute(
            "ALTER TABLE jobs ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0",
            [],
        ))?;
        // Migrate pre-existing DBs (created before `faults` was added). Earner-fault
        // rejects charge this counter instead of the dispatch `attempts` budget; an
        // existing job defaults to 0 faults, which is correct (none recorded yet).
        // Swallow only the duplicate-column error.
        ignore_duplicate_column(conn.execute(
            "ALTER TABLE jobs ADD COLUMN faults INTEGER NOT NULL DEFAULT 0",
            [],
        ))?;
        // Migrate pre-existing DBs (created before `dispatch_seq` was added). The
        // per-dispatch fence defaults to 0; the first `take_next` after a restart
        // bumps it to 1, so a migrated in-flight job that is recovered and
        // re-dispatched gets a fresh seq. Swallow only the duplicate-column error.
        ignore_duplicate_column(conn.execute(
            "ALTER TABLE jobs ADD COLUMN dispatch_seq INTEGER NOT NULL DEFAULT 0",
            [],
        ))?;
        // Migrate pre-existing DBs (created before `created_at` — the immutable
        // wall-clock-TTL anchor — was added). The column lands NULL on every
        // existing row; backfill those to now so a job already in the queue gets
        // a sane creation time and a finite TTL from this boot forward (rather
        // than a NULL the reaper would skip forever). New rows set it at enqueue.
        // Swallow only the duplicate-column error.
        ignore_duplicate_column(
            conn.execute("ALTER TABLE jobs ADD COLUMN created_at INTEGER", []),
        )?;
        conn.execute(
            "UPDATE jobs SET created_at = CAST(strftime('%s','now') AS INTEGER) WHERE created_at IS NULL",
            [],
        )?;
        // Migrate pre-existing DBs (created before `dispatched_to` — the WS-dispatch
        // holder address the liveness reaper keys on — was added). The column lands
        // NULL on every existing row, which is correct: a recovered/legacy in_flight
        // job has no recorded holder, so the liveness reaper skips it (NULL = "not
        // attributable, leave to the deadline reaper") and it falls back to the
        // deadline/TTL reapers exactly as before. Swallow only the duplicate-column error.
        ignore_duplicate_column(
            conn.execute("ALTER TABLE jobs ADD COLUMN dispatched_to TEXT", []),
        )?;
        // Migrate pre-existing DBs (created before `buyer` — the optional EVM
        // address charged for the job's validated compute via ComputeMeter — was
        // added). The column lands NULL on every existing row, which is correct: an
        // unattributed job simply isn't metered (the metering seam skips a NULL
        // buyer, mirroring the unknown-region render-fee skip). Set at enqueue for
        // jobs ingested with a buyer; the boot-seed and crash-recovery requeue leave
        // it NULL. Swallow only the duplicate-column error.
        ignore_duplicate_column(
            conn.execute("ALTER TABLE jobs ADD COLUMN buyer TEXT", []),
        )?;
        // Index the reaper's hot predicate. Every reap tick scans non-terminal jobs
        // by (status, age): the TTL reaper filters `status IN (queued, in_flight)
        // AND created_at IS NOT NULL`, the deadline/liveness reapers filter
        // `status = in_flight`. Without an index each tick is a full table scan that
        // grows with the never-archived terminal-row history; this turns it into an
        // index seek over only the live rows. Created AFTER the `created_at`
        // migration above so a legacy DB has the column before the index references
        // it; `IF NOT EXISTS` makes it idempotent across restarts (it builds once at
        // init on an existing DB, never per tick). `created_at` is immutable, so the
        // only index-write cost is one entry move per status transition.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_jobs_status_created_at ON jobs(status, created_at)",
            [],
        )?;
        // Cover the `/stats` lifetime SUM(attempts), SUM(faults) over jobs. The
        // table is never archived, so the unfiltered SUM scans one row per job EVER
        // — and each row carries the multi-KB `spec_json` inline, so the full table
        // scan pulls every spec blob through the b-tree just to read two integers.
        // This covering index lets the planner scan only the skinny (attempts,
        // faults, rowid) entries instead, a large constant-factor reduction in pages
        // read (the spec_json is never touched). NOTE: an unfiltered SUM is still
        // O(rows) — the index bounds the CONSTANT FACTOR, not the asymptote; a true
        // O(1) read would need a maintained running total, deliberately NOT taken
        // (attempts/faults mutate across dispatch/reap/earner-fault/restart, so a
        // side counter would have to update under every one of those in the same
        // transaction and rebuild on restart — a correctness risk the live-row SUM
        // does not carry, for a cosmetic stat). Keyed on neither `status` nor
        // `created_at`, so it does not perturb the reaper/dispatch/enqueue-cap plans
        // (asserted by their EXPLAIN tests). The only write cost is one skinny entry
        // move per attempts/faults bump, marginal beside the row write that bump
        // already does. Created AFTER the attempts/faults migrations above so a
        // legacy DB has both columns before the index references them.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_jobs_attempts_faults ON jobs(attempts, faults)",
            [],
        )?;
        // Migrate a legacy ROW-shaped earner_faults (the pre-counter table: one row
        // per fault event, carrying the write-only `job_id`/`created_at` columns) into
        // the counter shape (one row per earner, `fault_count` = its lifetime event
        // total). Detected by the legacy `job_id` column; the rebuild rolls each
        // earner's row COUNT into `fault_count`, preserving every earner's total
        // EXACTLY, then swaps the rebuilt table in — dropping the old table also drops
        // its now-redundant `idx_earner_faults_earner` (the prior covering index for
        // the GROUP BY; `earner` is the counter's PRIMARY KEY, so the read no longer
        // groups). Wrapped in one transaction so a crash mid-rebuild leaves the
        // original table intact. Runs once: after the swap the table has no `job_id`
        // column, so the guard is false on every later boot (idempotent).
        if table_has_column(&conn, "earner_faults", "job_id")? {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE earner_faults_counter (
                     earner      TEXT PRIMARY KEY,
                     fault_count INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO earner_faults_counter (earner, fault_count)
                     SELECT earner, COUNT(*) FROM earner_faults GROUP BY earner;
                 DROP TABLE earner_faults;
                 ALTER TABLE earner_faults_counter RENAME TO earner_faults;
                 COMMIT;",
            )?;
        }
        Ok(Self {
            conn,
            compute_rate_wei: 0,
        })
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
    ///
    /// `created_at` is stamped once on the initial insert and deliberately left
    /// untouched by the `ON CONFLICT` update: it is the immutable anchor for the
    /// absolute wall-clock TTL (see [`reap_ttl_expired`](Self::reap_ttl_expired)),
    /// so re-enqueueing the same id (e.g. an operator re-submit) cannot reset the
    /// clock and keep a stuck job alive forever.
    pub fn enqueue(&self, job: &JobSpec) -> Result<()> {
        let spec_json = serde_json::to_string(job)?;
        self.conn.execute(
            "INSERT INTO jobs (id, spec_json, status, created_at)
             VALUES (?1, ?2, ?3, CAST(strftime('%s','now') AS INTEGER))
             ON CONFLICT(id) DO UPDATE SET spec_json = ?2, status = ?3",
            (job.id.to_string(), spec_json, STATUS_QUEUED),
        )?;
        Ok(())
    }

    /// Enqueue `job` as `queued` ONLY if the current queued backlog is below
    /// `max_queued`; returns `true` if inserted, `false` if the cap was already
    /// full (nothing inserted). The runtime-ingestion backstop against an unbounded
    /// queued backlog (`POST /jobs`).
    ///
    /// The depth check and the insert are ONE statement (`INSERT ... SELECT ...
    /// WHERE (SELECT COUNT(*) ...) < ?`), so the count-then-insert is atomic with no
    /// TOCTOU window — two concurrent creators at `cap-1` can't both insert. The
    /// COUNT over `status = queued` is served by `idx_jobs_status_created_at`, and
    /// because this cap holds the backlog never exceeds `max_queued`, the count
    /// visits at most `max_queued` index entries — bounded, so a flood can't make
    /// each insert an O(table) scan and amplify the very DoS this guards.
    ///
    /// The count is derived from the live rows, never a side counter, so it tracks
    /// reality across every lifecycle transition that leaves `queued` (dispatch to
    /// in_flight, reap, dead-letter) with nothing to reconcile on restart. No
    /// `ON CONFLICT` clause: ingestion mints a fresh v4 id, so a collision is a
    /// genuine (astronomically unlikely) error, not a silent upsert. The boot-time
    /// `seed_jobs` and crash-recovery requeue use the uncapped `enqueue`, so they
    /// are exempt from this cap.
    ///
    /// `buyer` is the optional EVM address charged for the job's compute (NULL
    /// when the caller ingests it unattributed); it is the only buyer source, so
    /// the uncapped [`enqueue`](Self::enqueue) used by seed/recovery stays
    /// buyerless by construction.
    pub fn enqueue_within_cap(
        &self,
        job: &JobSpec,
        max_queued: usize,
        buyer: Option<&str>,
    ) -> Result<bool> {
        let spec_json = serde_json::to_string(job)?;
        let inserted = self.conn.execute(
            "INSERT INTO jobs (id, spec_json, status, created_at, buyer)
             SELECT ?1, ?2, ?3, CAST(strftime('%s','now') AS INTEGER), ?5
             WHERE (SELECT COUNT(*) FROM jobs WHERE status = ?3) < ?4",
            (job.id.to_string(), spec_json, STATUS_QUEUED, max_queued as i64, buyer),
        )?;
        Ok(inserted == 1)
    }

    /// The EVM address charged for this job's validated compute, or `None` if the
    /// job was ingested unattributed (or is unknown). Read at settle by
    /// [`record_completed`](Self::record_completed) to build the pending debit;
    /// `None` means the job is simply not metered. A NULL column or a missing row
    /// both map to `None`.
    pub fn job_buyer(&self, id: &uuid::Uuid) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT buyer FROM jobs WHERE id = ?1",
                [id.to_string()],
                |r| r.get::<_, Option<String>>(0),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .map_err(Into::into)
    }

    /// Pop the oldest-waiting queued job whose kind passes `accept`, marking it
    /// `in_flight` and stamping a fresh per-dispatch fence. Returns the job with
    /// its new `dispatch_seq`, or `None` if no queued job matches.
    ///
    /// `dispatch_seq` is a monotonic per-job counter bumped on every dispatch. It
    /// identifies *this* hand-out: the coordinator remembers it for the session
    /// and later requires it to match (`current_dispatch_seq`) before settling,
    /// requeueing, or sliding the deadline — so a job reaped and reassigned to a
    /// new earner can neither be settled nor preempted by the previous holder.
    ///
    /// Dispatch is oldest-first (FIFO by `created_at`, `rowid` as the stable
    /// tiebreaker for same-second inserts): the longest-waiting renderable job is
    /// handed out first so a steady arrival rate can't starve an old job until the
    /// wall-clock TTL reaps it. `created_at` is the immutable per-job anchor (a
    /// requeue never slides it), so a requeued job correctly returns to the front
    /// by age. The order is served by `idx_jobs_status_created_at(status,
    /// created_at)` — no temp-b-tree sort.
    ///
    /// The anonymous (HTTP) dispatch records no holder — see [`take_next_for`] for
    /// the WS variant that stamps the earner address so the liveness reaper can
    /// reclaim a stranded job.
    ///
    /// [`take_next_for`]: Self::take_next_for
    pub fn take_next<F>(&self, accept: F) -> Result<Option<(JobSpec, i64)>>
    where
        F: Fn(&JobSpec) -> bool,
    {
        self.take_next_inner(None, accept)
    }

    /// Like [`take_next`](Self::take_next), but records `holder` (the earner
    /// address) as the job's `dispatched_to` in the SAME atomic dispatch UPDATE, so
    /// the liveness reaper ([`reap_stale_holders`](Self::reap_stale_holders)) can
    /// reclaim the job promptly if that earner goes stale — instead of waiting for
    /// the full per-job deadline. The WS dispatcher uses this; the anonymous HTTP
    /// poll uses `take_next` (holder stays NULL → reaped only on the deadline path).
    pub fn take_next_for<F>(&self, holder: &str, accept: F) -> Result<Option<(JobSpec, i64)>>
    where
        F: Fn(&JobSpec) -> bool,
    {
        self.take_next_inner(Some(holder), accept)
    }

    /// Shared implementation of the two dispatch variants. `holder` is written to
    /// `dispatched_to` on the in_flight transition — `Some(addr)` for a WS dispatch,
    /// `None` (SQL NULL) for the anonymous HTTP poll. Because EVERY transition into
    /// `in_flight` goes through here, an in_flight job's `dispatched_to` is always
    /// the current holder (a prior value from an earlier dispatch is overwritten),
    /// so the reaper never needs to clear it on requeue.
    fn take_next_inner<F>(&self, holder: Option<&str>, accept: F) -> Result<Option<(JobSpec, i64)>>
    where
        F: Fn(&JobSpec) -> bool,
    {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_json, dispatch_seq FROM jobs WHERE status = ?1
             ORDER BY created_at ASC, rowid ASC",
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
                     SET status        = ?1,
                         started_at    = CAST(strftime('%s','now') AS INTEGER),
                         attempts      = attempts + 1,
                         dispatch_seq  = ?2,
                         dispatched_to = ?4
                     WHERE id = ?3",
                    (STATUS_IN_FLIGHT, new_seq, &id, holder),
                )?;
                return Ok(Some((job, new_seq)));
            }
        }
        Ok(None)
    }

    /// Put a job back on the queue after a deadline-miss / dropped-connection
    /// requeue — where the dispatch was a genuine (if failed) rendering attempt —
    /// or dead-letter it if it has already been dispatched `max_attempts` times,
    /// but ONLY while it is still `in_flight`. This is the *attempt-charging*
    /// requeue: it consumes the renderability budget `take_next` charged at
    /// dispatch. An EARNER-fault reject (bad signature, malformed/implausible
    /// content) must NOT burn that budget — route those to
    /// [`requeue_earner_fault`](Self::requeue_earner_fault) instead.
    ///
    /// This is an earner-driven action (its socket dropped, or a non-fault
    /// requeue). If the reaper has already requeued or dead-lettered the job, the
    /// late reject/disconnect is a no-op, so it can't resurrect a terminal job or
    /// re-queue one the reaper just parked. Reads `attempts` to decide:
    /// `>= max_attempts` → terminal `failed`, otherwise back to `queued`; clears
    /// `started_at`. Reaper-driven expiry lives in `reap_expired`.
    ///
    /// The `in_flight` check alone cannot distinguish "in_flight under me" from
    /// "in_flight under a later earner" (a job reaped then reassigned). That fence
    /// lives one layer up: the coordinator's `requeue` helper compares
    /// `current_dispatch_seq` against the seq it dispatched — under this same store
    /// lock — before calling here, so a stale holder's late disconnect can no
    /// longer preempt the new holder. This method is the `in_flight`/`attempts`
    /// backstop beneath that dispatch-seq fence.
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
        let Some((status, attempts)) = row else {
            return Ok(false);
        }; // unknown job
        if status != STATUS_IN_FLIGHT {
            return Ok(false); // already reaped or terminal — don't clobber
        }

        let new_status = if attempts >= max_attempts {
            STATUS_FAILED
        } else {
            STATUS_QUEUED
        };
        // Guard the transition on in_flight too, so it stays atomic with the read.
        self.conn.execute(
            "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2 AND status = ?3",
            (new_status, job.id.to_string(), STATUS_IN_FLIGHT),
        )?;
        Ok(new_status == STATUS_FAILED)
    }

    /// Return a job to the queue after an EARNER-fault result reject (bad/forged
    /// signature, malformed/implausible content, or a submit-protocol violation)
    /// WITHOUT consuming a dispatch attempt: the job itself is still renderable,
    /// so only the faulting earner should be penalized, never the job. The
    /// dispatch attempt that `take_next` provisionally charged is refunded
    /// (`attempts -= 1`) and a separate `faults` counter is charged instead. The
    /// job is dead-lettered only once it has accumulated `max_faults` earner
    /// faults — the backstop that still terminates a poison job (a spec no earner
    /// can satisfy) while a renderable job can no longer be burned to `failed` by
    /// an earner that keeps submitting garbage. Termination is at the fault-count
    /// level: the ws layer caps each session's contribution to one fault per job
    /// (the per-session skip set), so reaching `max_faults` takes faults from
    /// `max_faults` distinct sessions/earners — a single connected earner parks
    /// the job at one fault rather than dead-lettering it, by design.
    ///
    /// Like [`requeue`](Self::requeue) this acts ONLY on a still-`in_flight` job
    /// and is a no-op otherwise (a reaper-parked or terminal job is left
    /// untouched). The dispatch-seq fence one layer up (the coordinator `requeue`
    /// helper) has already confirmed, under this same store lock, that we still
    /// hold the current dispatch — so the read+update here is atomic against any
    /// other dispatch, exactly as in `requeue`.
    ///
    /// `attribute_to`, when `Some(earner)`, records a per-earner reputation fault
    /// via [`record_earner_fault`](Self::record_earner_fault) — but ONLY past the
    /// `in_flight` guard below, so it shares the EXACT condition under which the
    /// `jobs.faults` budget is bumped. This is load-bearing: the seq-fence one
    /// layer up passes for a job a reaper has parked back to `queued` at the SAME
    /// seq (reapers don't bump `dispatch_seq`), so attributing in the caller would
    /// over-count a fault the per-job budget no-ops on, breaking the
    /// `Σ attributed <= total_faults` invariant. Co-gating here keeps the two in
    /// lockstep. An honest `Decline` passes `None` and is never attributed.
    ///
    /// Returns `true` iff the job was dead-lettered (moved to `failed`); `false`
    /// if it was requeued OR a no-op (not in_flight / unknown).
    pub fn requeue_earner_fault(
        &self,
        job: &JobSpec,
        max_faults: u32,
        attribute_to: Option<&str>,
    ) -> Result<bool> {
        let row: Option<(String, u32)> = self
            .conn
            .query_row(
                "SELECT status, faults FROM jobs WHERE id = ?1",
                [job.id.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some((status, faults)) = row else {
            return Ok(false);
        }; // unknown job
        if status != STATUS_IN_FLIGHT {
            return Ok(false); // already reaped or terminal — don't clobber or attribute
        }

        // Past the in_flight guard, the fault IS charged — attribute it (if named)
        // in the same locked, no-await path as the budget bump below so the
        // per-earner tally can never diverge from the per-job faults counter.
        if let Some(earner) = attribute_to {
            self.record_earner_fault(earner)?;
        }

        let new_faults = faults + 1;
        let new_status = if new_faults >= max_faults {
            STATUS_FAILED
        } else {
            STATUS_QUEUED
        };
        // Refund the dispatch attempt `take_next` charged (this dispatch was an
        // earner fault, not a real rendering attempt) and record the fault, in one
        // UPDATE guarded on in_flight so it stays atomic with the read above.
        // `MAX(attempts - 1, 0)` floors the refund — an in_flight job always has
        // attempts >= 1, so this only guards a hypothetical underflow.
        self.conn.execute(
            "UPDATE jobs
             SET status = ?1, started_at = NULL, attempts = MAX(attempts - 1, 0), faults = ?2
             WHERE id = ?3 AND status = ?4",
            (new_status, new_faults, job.id.to_string(), STATUS_IN_FLIGHT),
        )?;
        Ok(new_status == STATUS_FAILED)
    }

    /// Record one GENUINE earner quality fault for reputation attribution: the
    /// dispatch holder `earner` returned a faulty result (bad/forged/replayed
    /// signature, malformed/implausible content) or violated the submit protocol
    /// (submit-before-accept, job_id mismatch) on `job`. Surfaced per earner on
    /// the `/earners` leaderboard via [`faults_by_earner`](Self::faults_by_earner).
    ///
    /// Unlike the per-JOB `jobs.faults` budget that `requeue_earner_fault` bumps,
    /// this is keyed by earner and is NEVER written for an honest
    /// `EarnerMsg::Decline` of an unsupported kind — declining a kind you cannot
    /// serve is correct, anti-hot-loop behavior, not a reputation fault. Increments
    /// the earner's lifetime tally by one fault EVENT (no dedup): the same earner
    /// faulting the same job across reconnects is two genuine bad submissions and
    /// counts twice. The counter is the single source of truth — there is no
    /// per-fault row to diverge from — so the bump is one atomic UPSERT.
    pub fn record_earner_fault(&self, earner: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO earner_faults (earner, fault_count) VALUES (?1, 1)
             ON CONFLICT(earner) DO UPDATE SET fault_count = fault_count + 1",
            [earner],
        )?;
        Ok(())
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
        // Settle-policy inputs, read before the tx: the compute rate (Copy) and the
        // job's buyer. Buyer is write-once at enqueue and never mutated, so it needs
        // no transactional snapshot — and reading it via the accessor inside the tx
        // would double-borrow the connection. Both feed the pending debit below.
        let compute_rate_wei = self.compute_rate_wei;
        let buyer = self.job_buyer(&result.job_id)?;
        let tx = self.conn.transaction()?;

        // Re-check lifecycle inside the transaction; only an in_flight job is
        // settle-able. A non-in_flight (queued/done/failed) or unknown job is
        // refused, leaving its state untouched. The spec is read alongside the
        // status (same row) to build the pending attestation below.
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT status, spec_json FROM jobs WHERE id = ?1",
                [&job_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some((status, spec_json)) = row else {
            return Ok(false);
        };
        if status != STATUS_IN_FLIGHT {
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
        // Record the pending EAS render receipt in the SAME transaction as the
        // settle, so a crash before the on-chain relay cannot leave a settled job
        // un-attested. Built from the stored spec (kind + region) + the result;
        // the ON CONFLICT keeps a replay a no-op (the in_flight guard already
        // blocks a second settle). Skipped only if the result's output_hash is
        // malformed — unreachable on the content-gated path, but record_completed
        // is also reachable directly (tests), so we degrade rather than fail.
        match serde_json::from_str::<JobSpec>(&spec_json)
            .ok()
            .and_then(|spec| crate::eas::PendingAttestation::build(&spec, result))
        {
            Some(att) => {
                tx.execute(
                    "INSERT INTO pending_attestations
                         (job_id, earner, job_id_b32, render_seconds, job_kind, output_hash, region_id_b32, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(job_id) DO NOTHING",
                    (
                        &job_id,
                        &att.earner,
                        &att.job_id,
                        att.render_seconds as i64,
                        att.job_kind,
                        &att.output_hash,
                        &att.region_id,
                        created_at,
                    ),
                )?;
            }
            None => {
                tracing::warn!(%job_id, "settle: spec unreadable or output_hash malformed; pending attestation skipped");
            }
        }
        // Record the pending ComputeMeter debit in the SAME settle tx (the metering
        // twin of the pending attestation above) so a crash before the on-chain
        // spend cannot lose the charge. build() returns None — no row, never an
        // error — when there is no buyer to charge or the amount is zero (metering
        // disabled or zero render-seconds), so an unmetered job still settles and
        // is still attested. ON CONFLICT keeps a replay a no-op.
        if let Some(debit) =
            crate::meter::PendingDebit::build(buyer.as_deref(), result, compute_rate_wei)
        {
            tx.execute(
                "INSERT INTO pending_debits
                     (job_id, buyer, amount_wei, job_id_b32, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(job_id) DO NOTHING",
                (
                    &job_id,
                    &debit.buyer,
                    &debit.amount_wei,
                    &debit.job_id,
                    created_at,
                ),
            )?;
        }
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

    /// Dead-letter any non-terminal job alive past its absolute wall-clock TTL.
    ///
    /// This is the poison-job backstop that neither the per-dispatch deadline
    /// reaper nor the attempt/fault budgets can reach: a job a single connected
    /// earner keeps faulting on parks back in `queued` with one fault (the
    /// per-session skip set caps each earner at one), so `reap_expired`
    /// (in_flight only) never sees it and `max_faults` — which needs faults from
    /// `max_faults` *distinct* earners — is never reached. Anchored to the
    /// immutable `created_at`, a job's TTL is `deadline_secs * ttl_multiple`; any
    /// `queued` OR `in_flight` job whose age exceeds it is moved to the terminal
    /// `failed` state regardless of attempts, faults, or earner count.
    ///
    /// * `deadline_secs == 0` (operator-unbounded) is exempt — never reaped here,
    ///   mirroring how the deadline reaper treats it as "no deadline".
    /// * `ttl_multiple` makes the TTL dominate the per-dispatch deadline, so a
    ///   long-but-healthy render — even one that heartbeats across many redispatch
    ///   cycles — never trips it; only a genuinely stuck job does. The caller sets
    ///   it well above `max_attempts + max_faults` so the existing budgets always
    ///   terminate a churning job first.
    /// * The UPDATE is guarded `WHERE status IN (queued, in_flight)` so a job that
    ///   raced to `done`/`failed` between the scan and the write is a no-op; only
    ///   rows actually transitioned are reported.
    ///
    /// `now_secs` is passed in (epoch seconds) so callers — and tests — control
    /// the clock. Returns the ids dead-lettered this sweep.
    pub fn reap_ttl_expired(&self, now_secs: i64, ttl_multiple: u32) -> Result<Vec<uuid::Uuid>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_json, created_at FROM jobs
             WHERE status IN (?1, ?2) AND created_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([STATUS_QUEUED, STATUS_IN_FLIGHT], |row| {
            let id: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            let created_at: i64 = row.get(2)?;
            Ok((id, spec_json, created_at))
        })?;

        let mut expired = Vec::new();
        for row in rows {
            let (id, spec_json, created_at) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            if job.deadline_secs == 0 {
                continue; // operator-unbounded job: no wall-clock TTL
            }
            // saturating, not checked: an extreme deadline_secs/multiple must not
            // panic — this runs inside the background reaper, where an unwind would
            // silently kill the whole reap loop (the caller's match only catches Err,
            // not a panic). A saturated TTL just means "effectively never expires".
            let ttl = (job.deadline_secs as i64).saturating_mul(ttl_multiple as i64);
            if now_secs - created_at >= ttl {
                // Guard on a still-non-terminal status so a settle/dead-letter
                // that landed since the scan above is left untouched.
                let changed = self.conn.execute(
                    "UPDATE jobs SET status = ?1, started_at = NULL
                     WHERE id = ?2 AND status IN (?3, ?4)",
                    (STATUS_FAILED, &id, STATUS_QUEUED, STATUS_IN_FLIGHT),
                )?;
                if changed > 0 {
                    expired.push(job.id);
                }
            }
        }
        Ok(expired)
    }

    /// Requeue (or dead-letter) in-flight jobs whose recorded WS holder is no
    /// longer live, reclaiming a job stranded by a power-failed earner on the
    /// earner-TTL timescale instead of waiting for the full per-job deadline.
    ///
    /// A job is reaped only when ALL hold:
    /// * `dispatched_to IS NOT NULL` — it was dispatched over the WS path, which
    ///   records the holder. An anonymous HTTP poll leaves `dispatched_to` NULL and
    ///   is intentionally skipped here (it stays on the deadline reaper) — treating
    ///   NULL as "no live holder" would requeue every HTTP-dispatched job every
    ///   tick, an infinite churn.
    /// * `dispatched_to NOT IN live` — the holder has dropped out of the in-memory
    ///   registry's live set (silent past `earner_ttl_secs`). A still-heartbeating
    ///   earner stays in `live` and is left alone.
    /// * `now_secs - started_at >= grace_secs` — the job has been untouched (no
    ///   dispatch/heartbeat) for at least the grace. `started_at` is bumped on every
    ///   heartbeat (`touch`), so a healthy earner mid-render never trips this; the
    ///   grace also stops a just-dispatched job from being reaped on a transient
    ///   registry gap. Set `grace_secs == earner_ttl_secs` so both staleness signals
    ///   align on the same timescale.
    ///
    /// Disposition mirrors [`reap_expired`](Self::reap_expired): the dispatch
    /// attempt was already charged by `take_next`, so a holder at/over `max_attempts`
    /// dead-letters to `failed`, otherwise the job returns to `queued`. The UPDATE is
    /// guarded `WHERE status = in_flight`, so a settle that raced in since the scan
    /// is a no-op (the original earner can never double-settle a reclaimed job) and a
    /// job already requeued by the disconnect path is not double-requeued. The
    /// dispatch_seq fence one layer up still blocks the reaped holder's late settle
    /// after the job is reassigned to a new earner.
    ///
    /// `now_secs` is passed in (epoch seconds) so callers — and tests — control the
    /// clock. The `live` set is snapshotted by the caller WITHOUT holding the store
    /// lock (the registry and the store have separate locks; see the reaper).
    pub fn reap_stale_holders(
        &self,
        live: &HashSet<String>,
        now_secs: i64,
        grace_secs: i64,
        max_attempts: u32,
    ) -> Result<ReapOutcome> {
        let mut stmt = self.conn.prepare(
            "SELECT id, spec_json, started_at, attempts, dispatched_to FROM jobs
             WHERE status = ?1 AND dispatched_to IS NOT NULL AND started_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([STATUS_IN_FLIGHT], |row| {
            let id: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            let started_at: i64 = row.get(2)?;
            let attempts: u32 = row.get(3)?;
            let holder: String = row.get(4)?;
            Ok((id, spec_json, started_at, attempts, holder))
        })?;

        let mut outcome = ReapOutcome::default();
        for row in rows {
            let (id, spec_json, started_at, attempts, holder) = row?;
            if live.contains(&holder) {
                continue; // holder still heartbeating — leave its render alone
            }
            if now_secs - started_at < grace_secs {
                continue; // within grace — tolerate a transient registry gap
            }
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            let new_status = if attempts >= max_attempts {
                STATUS_FAILED
            } else {
                STATUS_QUEUED
            };
            // Guard on in_flight so a settle/reassign that landed since the scan is a
            // no-op, exactly as in reap_expired/requeue.
            let changed = self.conn.execute(
                "UPDATE jobs SET status = ?1, started_at = NULL WHERE id = ?2 AND status = ?3",
                (new_status, &id, STATUS_IN_FLIGHT),
            )?;
            if changed > 0 {
                if new_status == STATUS_FAILED {
                    outcome.failed.push(job.id);
                } else {
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

    /// Delete a bounded batch of aged terminal jobs and every row that hangs off
    /// them, bounding the otherwise-unbounded growth of the `jobs` history (and its
    /// `results` / `pending_attestations` / `pending_debits` dependents). The
    /// covering indexes made the `/stats` aggregates a smaller constant factor but
    /// left them O(rows-ever); retention is the asymptotic fix — it caps the row
    /// count itself.
    ///
    /// A job is pruned only when ALL hold, so the sweep can never drop a row still
    /// needed for correctness or accounting:
    /// * its status is terminal (`done` or `failed`) — a `queued`/`in_flight` job is
    ///   live work and is left to the reapers, never deleted by retention;
    /// * `created_at <= now_secs - horizon_secs` — it has aged past the retention
    ///   window, anchored to the immutable enqueue time (like
    ///   [`reap_ttl_expired`](Self::reap_ttl_expired)). A terminal job never mutates
    ///   again and the horizon is far longer than any job's wall-clock TTL, so
    ///   `created_at` is within a bounded margin of the true terminal time — close
    ///   enough for a storage backstop, with no new column or terminal-transition
    ///   stamp to maintain;
    /// * it carries NO still-pending on-chain obligation — no `pending_attestations`
    ///   row with `uid IS NULL` and no `pending_debits` row with `tx_hash IS NULL`.
    ///   A settled job's EAS receipt / ComputeMeter debit must land on chain before
    ///   its record may be discarded; until the relayer drains them the job is kept
    ///   (so with no relayer configured — the dev default — settled jobs are
    ///   retained regardless of age, and only `failed` jobs prune).
    ///
    /// All four tables are keyed by the job id, so the dependent `results` /
    /// `pending_attestations` / `pending_debits` rows are deleted in the SAME
    /// transaction as the job — `/stats` aggregates that read across them
    /// (`payout_wei_by_earner` joins results↔jobs; `completed_count` counts results)
    /// stay mutually consistent, never left with an orphan that skews one total.
    /// Under retention those lifetime aggregates therefore report the retained
    /// window, not all-time.
    ///
    /// Bounded to `batch` jobs per call so the caller can release the store lock
    /// between batches (dispatch/settle never stall behind a large delete); the
    /// caller loops until a call prunes `< batch` (the backlog is drained) or a
    /// per-tick batch cap is hit. The candidate scan is served by
    /// `idx_jobs_status_created_at` (a range over only the aged terminal rows), so a
    /// tick that finds nothing to prune costs an index seek, not a full scan of the
    /// history it exists to bound. Returns the number of jobs pruned.
    pub fn prune_terminal_jobs(
        &mut self,
        now_secs: i64,
        horizon_secs: i64,
        batch: usize,
    ) -> Result<usize> {
        let cutoff = now_secs.saturating_sub(horizon_secs);
        let tx = self.conn.transaction()?;
        // Collect a bounded batch of prunable ids first (the NOT IN subqueries
        // exclude jobs whose receipt/debit is still pending), then delete each job
        // and its dependents. Selecting ids up front keeps the candidate statement
        // and the delete statements from co-borrowing the transaction.
        let ids: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM jobs
                 WHERE status IN (?1, ?2)
                   AND created_at IS NOT NULL
                   AND created_at <= ?3
                   AND id NOT IN (SELECT job_id FROM pending_attestations WHERE uid IS NULL)
                   AND id NOT IN (SELECT job_id FROM pending_debits WHERE tx_hash IS NULL)
                 LIMIT ?4",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![STATUS_DONE, STATUS_FAILED, cutoff, batch as i64],
                |r| r.get::<_, String>(0),
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if ids.is_empty() {
            return Ok(0);
        }
        {
            let mut del_results = tx.prepare("DELETE FROM results WHERE job_id = ?1")?;
            let mut del_att = tx.prepare("DELETE FROM pending_attestations WHERE job_id = ?1")?;
            let mut del_debit = tx.prepare("DELETE FROM pending_debits WHERE job_id = ?1")?;
            let mut del_job = tx.prepare("DELETE FROM jobs WHERE id = ?1")?;
            for id in &ids {
                del_results.execute([id])?;
                del_att.execute([id])?;
                del_debit.execute([id])?;
                del_job.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(ids.len())
    }

    /// Test-only: overwrite a job's `created_at` anchor so a test can build a
    /// deterministic age ordering that disagrees with `rowid` (the live `enqueue`
    /// stamps wall-clock seconds, which tie within a fast test). Returns whether a
    /// row was updated.
    #[cfg(test)]
    pub fn set_created_at(&self, id: &uuid::Uuid, created_at: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE jobs SET created_at = ?2 WHERE id = ?1",
            (id.to_string(), created_at),
        )?;
        Ok(n == 1)
    }

    /// Immutable creation timestamp (epoch seconds) of a job, or `None` if the
    /// id is unknown (or the row predates the column and was never backfilled).
    /// Test-only: lets the TTL tests assert the anchor is set at enqueue and is
    /// not slid by requeue/fault/reap.
    #[cfg(test)]
    pub fn job_created_at(&self, id: &uuid::Uuid) -> Result<Option<i64>> {
        let created_at = self
            .conn
            .query_row(
                "SELECT created_at FROM jobs WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(created_at)
    }

    /// Recorded WS holder (`dispatched_to`) of a job, or `None` if the id is
    /// unknown OR the column is NULL (an anonymous HTTP dispatch / never dispatched).
    /// Test-only: lets the liveness-reap tests assert the holder is stamped by
    /// `take_next_for` and left NULL by `take_next`.
    #[cfg(test)]
    pub fn job_dispatched_to(&self, id: &uuid::Uuid) -> Result<Option<String>> {
        let holder = self
            .conn
            .query_row(
                "SELECT dispatched_to FROM jobs WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        Ok(holder)
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

    /// Count of jobs whose renderability budget shows more than one dispatch
    /// (`attempts > 1`): `take_next` handed them out, the reaper requeued them on
    /// a missed deadline, and they were dispatched again. A cumulative
    /// reaper-churn signal for `/stats` — counts each such job once regardless of
    /// current status. `attempts` is bumped on every dispatch but *refunded* by an
    /// earner-fault requeue (those charge the separate `faults` budget), so this
    /// tracks deadline/disconnect-driven redispatches, not earner-fault churn.
    /// Zero on a healthy mesh where every job lands on its first dispatch.
    pub fn redispatched_count(&self) -> Result<usize> {
        let count: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM jobs WHERE attempts > 1", [], |row| {
                    row.get(0)
                })?;
        Ok(count as usize)
    }

    /// Gross `(total_attempts, total_faults)` across every job, summed in one
    /// table scan for `/stats`. `total_attempts` is Σ`attempts` — the dispatch
    /// count currently charged to each job's renderability budget (each
    /// `take_next` bumps it; an earner-fault requeue refunds it, so it is net of
    /// earner faults), summed across jobs. It is the gross redispatch volume, NOT
    /// the count of distinct redispatched jobs (`redispatched_count`): one job
    /// dispatched five times adds 5 here but 1 there. `total_faults` is Σ`faults` — every
    /// earner-fault reject charged on the budget separate from attempts. Together
    /// they let an operator separate reaper/disconnect churn (attempts) from
    /// earner-quality problems (faults). `COALESCE(…, 0)` makes an empty table
    /// report `(0, 0)` rather than NULL; widened to `u64` (mirroring
    /// `total_render_seconds`) so a large mesh can't overflow the per-row `u32`.
    /// Same cost class as the sibling `/stats` aggregates — a single scan, cheaper
    /// than `total_payout_wei`, which JSON-decodes every done spec.
    pub fn attempt_fault_totals(&self) -> Result<(u64, u64)> {
        let (attempts, faults): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(attempts), 0), COALESCE(SUM(faults), 0) FROM jobs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((attempts as u64, faults as u64))
    }

    /// Count of recorded results (for `/stats`).
    pub fn completed_count(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM results", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Number of settled jobs whose EAS render receipt has not yet been relayed
    /// on-chain — the attestation backlog depth, surfaced at `/stats`. A row is
    /// pending while its `uid` is NULL; the relayer drains the backlog by writing
    /// the on-chain attestation `uid`, so this count falls as receipts land.
    pub fn pending_attestation_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pending_attestations WHERE uid IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Number of settled jobs whose ComputeMeter debit has not yet been spent
    /// on-chain — the debit backlog depth, surfaced at `/stats` (the metering twin
    /// of [`pending_attestation_count`](Self::pending_attestation_count)). A row is
    /// pending while its `tx_hash` is NULL; the relayer drains the backlog by
    /// writing the spend `tx_hash`, so this count falls as debits land.
    pub fn pending_debit_count(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pending_debits WHERE tx_hash IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// The oldest still-pending debit (`tx_hash IS NULL`), oldest-first by insert
    /// order, as `(job_id, PendingDebit)` — or `None` when the backlog is empty.
    /// The metering twin of [`claim_oldest_pending`](Self::claim_oldest_pending): a
    /// pure read that does NOT reserve or mutate the row, so the drain loop drops
    /// the store lock before the slow on-chain `spend` and only re-acquires it to
    /// `mark_debit_submitted`. A single drain task is the only caller and settles
    /// only ever INSERT new pending rows, so no reservation is needed to avoid a
    /// double-claim.
    pub fn claim_oldest_pending_debit(
        &self,
    ) -> Result<Option<(uuid::Uuid, crate::meter::PendingDebit)>> {
        let row = self.conn.query_row(
            "SELECT job_id, buyer, amount_wei, job_id_b32
             FROM pending_debits
             WHERE tx_hash IS NULL
             ORDER BY created_at ASC, rowid ASC
             LIMIT 1",
            [],
            |r| {
                let job_id: String = r.get(0)?;
                Ok((
                    job_id,
                    crate::meter::PendingDebit {
                        buyer: r.get(1)?,
                        amount_wei: r.get(2)?,
                        job_id: r.get(3)?,
                    },
                ))
            },
        );
        match row {
            Ok((job_id, debit)) => {
                let uuid = uuid::Uuid::parse_str(&job_id).map_err(|e| {
                    anyhow::anyhow!("pending_debits.job_id not a uuid {job_id:?}: {e}")
                })?;
                Ok(Some((uuid, debit)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Mark a pending debit spent on-chain by writing its `tx_hash` +
    /// `submitted_at`, but ONLY while it is still pending (`tx_hash IS NULL`).
    /// Returns whether a row was updated. The `tx_hash IS NULL` guard makes a
    /// re-mark after a crash-recovery re-submit a no-op, so a debit is never marked
    /// (or counted as drained) twice. The metering twin of
    /// [`mark_submitted`](Self::mark_submitted).
    pub fn mark_debit_submitted(
        &self,
        job_id: &uuid::Uuid,
        tx_hash: &str,
        now_secs: i64,
    ) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE pending_debits SET tx_hash = ?1, submitted_at = ?2
             WHERE job_id = ?3 AND tx_hash IS NULL",
            (tx_hash, now_secs, job_id.to_string()),
        )?;
        Ok(updated > 0)
    }

    /// The oldest still-pending attestation (`uid IS NULL`), oldest-first by
    /// insert order, as `(job_id, PendingAttestation)` — or `None` when the
    /// backlog is empty. A pure read: it does NOT reserve or mutate the row, so
    /// the drain loop drops the store lock before the slow on-chain submit and
    /// only re-acquires it to `mark_submitted`. A single drain task is the only
    /// caller and settles only ever INSERT new pending rows, so no reservation is
    /// needed to avoid a double-claim.
    pub fn claim_oldest_pending(
        &self,
    ) -> Result<Option<(uuid::Uuid, crate::eas::PendingAttestation)>> {
        let row = self.conn.query_row(
            "SELECT job_id, earner, job_id_b32, render_seconds, job_kind, output_hash, region_id_b32
             FROM pending_attestations
             WHERE uid IS NULL
             ORDER BY created_at ASC, rowid ASC
             LIMIT 1",
            [],
            |r| {
                let job_id: String = r.get(0)?;
                Ok((
                    job_id,
                    crate::eas::PendingAttestation {
                        earner: r.get(1)?,
                        job_id: r.get(2)?,
                        render_seconds: r.get::<_, i64>(3)? as u64,
                        job_kind: r.get::<_, i64>(4)? as u16,
                        output_hash: r.get(5)?,
                        region_id: r.get(6)?,
                    },
                ))
            },
        );
        match row {
            Ok((job_id, att)) => {
                let uuid = uuid::Uuid::parse_str(&job_id).map_err(|e| {
                    anyhow::anyhow!("pending_attestations.job_id not a uuid {job_id:?}: {e}")
                })?;
                Ok(Some((uuid, att)))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Mark a pending attestation relayed by writing its on-chain `uid` +
    /// `submitted_at`, but ONLY while it is still pending (`uid IS NULL`). Returns
    /// whether a row was updated. The `uid IS NULL` guard makes a re-mark after a
    /// crash-recovery re-submit a no-op, so a receipt is never marked (or counted
    /// as drained) twice.
    pub fn mark_submitted(&self, job_id: &uuid::Uuid, uid: &str, now_secs: i64) -> Result<bool> {
        let updated = self.conn.execute(
            "UPDATE pending_attestations SET uid = ?1, submitted_at = ?2
             WHERE job_id = ?3 AND uid IS NULL",
            (uid, now_secs, job_id.to_string()),
        )?;
        Ok(updated > 0)
    }

    /// Test-only: read back the pending attestation recorded for a job, rebuilt
    /// as an `eas::PendingAttestation` so a test can assert the settle-time
    /// mapping round-trips. `None` when no pending row exists for the job.
    #[cfg(test)]
    pub fn pending_attestation(
        &self,
        job_id: &uuid::Uuid,
    ) -> Result<Option<crate::eas::PendingAttestation>> {
        let row = self.conn.query_row(
            "SELECT earner, job_id_b32, render_seconds, job_kind, output_hash, region_id_b32
             FROM pending_attestations WHERE job_id = ?1",
            [&job_id.to_string()],
            |r| {
                Ok(crate::eas::PendingAttestation {
                    earner: r.get::<_, String>(0)?,
                    job_id: r.get::<_, String>(1)?,
                    render_seconds: r.get::<_, i64>(2)? as u64,
                    job_kind: r.get::<_, i64>(3)? as u16,
                    output_hash: r.get::<_, String>(4)?,
                    region_id: r.get::<_, String>(5)?,
                })
            },
        );
        match row {
            Ok(a) => Ok(Some(a)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Test-only: read back the pending debit recorded for a job, rebuilt as a
    /// `meter::PendingDebit` so a test can assert the settle-time mapping
    /// round-trips. `None` when no pending row exists for the job.
    #[cfg(test)]
    pub fn pending_debit(
        &self,
        job_id: &uuid::Uuid,
    ) -> Result<Option<crate::meter::PendingDebit>> {
        let row = self.conn.query_row(
            "SELECT buyer, amount_wei, job_id_b32 FROM pending_debits WHERE job_id = ?1",
            [&job_id.to_string()],
            |r| {
                Ok(crate::meter::PendingDebit {
                    buyer: r.get::<_, String>(0)?,
                    amount_wei: r.get::<_, String>(1)?,
                    job_id: r.get::<_, String>(2)?,
                })
            },
        );
        match row {
            Ok(d) => Ok(Some(d)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
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
        let mut stmt = self
            .conn
            .prepare("SELECT earner, result_json FROM results")?;
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

    /// Genuine quality-fault tally per earner (`earner_faults.fault_count`), for
    /// the `GET /earners` leaderboard `faults` field. Counts only attributed
    /// quality faults (honest declines are never recorded), so an earner with a
    /// clean record — or any earner on a fresh mesh — is simply absent from the
    /// map (the caller defaults it to 0). Empty map when no faults recorded. One
    /// row per earner, so this is a plain scan of a table bounded by the distinct
    /// faulting-earner count — no GROUP BY, no temp b-tree.
    pub fn faults_by_earner(&self) -> Result<HashMap<String, usize>> {
        let mut stmt = self
            .conn
            .prepare("SELECT earner, fault_count FROM earner_faults")?;
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

    /// Test-only: the physical row count of `earner_faults`, to prove the counter
    /// stays bounded by the distinct faulting-earner count (one row per earner) and
    /// not by total faults ever recorded.
    #[cfg(test)]
    pub fn earner_faults_row_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM earner_faults", [], |r| r.get(0))?)
    }

    /// Sum of `max_payout_wei` across all DONE jobs — the HUD "total $BLCKFLD
    /// payable" metric. `max_payout_wei` is a 1e18-scale decimal string on the
    /// `JobSpec`, so each done job's spec is decoded and its value parsed as
    /// `u128` (wei; u128 max ≈ 3.4e38 ≈ 3.4e20 ether — ample headroom). Summed
    /// with `checked_add` so an implausible overflow errors rather than wraps.
    /// Zero when nothing has completed.
    pub fn total_payout_wei(&self) -> Result<u128> {
        let mut stmt = self
            .conn
            .prepare("SELECT spec_json FROM jobs WHERE status = ?1")?;
        let rows = stmt.query_map([STATUS_DONE], |row| row.get::<_, String>(0))?;
        let mut total: u128 = 0;
        for row in rows {
            let job: JobSpec = serde_json::from_str(&row?)?;
            let wei: u128 = job.max_payout_wei.parse().map_err(|e| {
                anyhow::anyhow!("invalid max_payout_wei {:?}: {e}", job.max_payout_wei)
            })?;
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
        let mut stmt = self.conn.prepare(
            "SELECT r.earner, j.spec_json FROM results r JOIN jobs j ON r.job_id = j.id",
        )?;
        let rows = stmt.query_map([], |row| {
            let earner: String = row.get(0)?;
            let spec_json: String = row.get(1)?;
            Ok((earner, spec_json))
        })?;
        let mut totals: HashMap<String, u128> = HashMap::new();
        for row in rows {
            let (earner, spec_json) = row?;
            let job: JobSpec = serde_json::from_str(&spec_json)?;
            let wei: u128 = job.max_payout_wei.parse().map_err(|e| {
                anyhow::anyhow!("invalid max_payout_wei {:?}: {e}", job.max_payout_wei)
            })?;
            let slot = totals.entry(earner).or_insert(0u128);
            *slot = slot
                .checked_add(wei)
                .ok_or_else(|| anyhow::anyhow!("payout_wei_by_earner overflowed u128"))?;
        }
        Ok(totals)
    }
}
