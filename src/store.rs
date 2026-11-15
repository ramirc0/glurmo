//! History on disk: every attempt, five-minute usage bins, node health changes and user colors.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::Deserialize;

use crate::rollup::{BIN_S, Span, Usage, bin_of, rollup};
use rtviz_model::usage::Step;
use rtviz_model::{
    ArrayTasks, Attempt, AttemptKey, EndState, Event, JobId, JobState, Login, Node, NodeName,
    NodeState, PartitionName, Placement, UnixTime, User,
};

const DAY_S: i64 = 86_400;

/// How long after a bin closes the live path rolls it up, so ends reported a tick late still land in it.
const GRACE_S: i64 = 30;

/// Schema changes, applied in order. `PRAGMA user_version` counts how many have run.
const MIGRATIONS: [&str; 2] = [
    "
    CREATE TABLE attempts (
        job INTEGER NOT NULL,
        submit INTEGER NOT NULL,
        array_job INTEGER,
        task_range TEXT,
        tasks INTEGER NOT NULL,
        login TEXT NOT NULL,
        partition TEXT NOT NULL,
        cpus INTEGER NOT NULL,
        cores INTEGER,
        gpus INTEGER NOT NULL,
        mem_mb INTEGER,
        start INTEGER,
        end INTEGER,
        state TEXT,
        nodes TEXT NOT NULL,
        placement TEXT,
        exact INTEGER,
        origin TEXT NOT NULL,
        PRIMARY KEY (job, submit)
    ) WITHOUT ROWID;
    CREATE INDEX attempts_by_end ON attempts (end);
    CREATE INDEX open_attempts ON attempts (submit) WHERE end IS NULL;
    CREATE TABLE usage (
        bin INTEGER NOT NULL,
        login TEXT NOT NULL,
        partition TEXT NOT NULL,
        core_s INTEGER NOT NULL,
        gpu_s INTEGER NOT NULL,
        wait_s INTEGER NOT NULL,
        started INTEGER NOT NULL,
        ended INTEGER NOT NULL,
        failed INTEGER NOT NULL,
        PRIMARY KEY (bin, login, partition)
    ) WITHOUT ROWID;
    CREATE TABLE node_states (
        node TEXT NOT NULL,
        at INTEGER NOT NULL,
        state TEXT NOT NULL,
        reason TEXT,
        PRIMARY KEY (node, at)
    );
    CREATE TABLE users (login TEXT PRIMARY KEY, full_name TEXT, color_slot INTEGER NOT NULL);
    CREATE TABLE meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
",
    "
    CREATE INDEX attempts_by_start ON attempts (start);
    CREATE INDEX attempts_by_submit ON attempts (submit);
",
];

/// How much history to fetch and keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Days of accounting to backfill when the store is new.
    pub backfill_days: u32,
    /// Days of history to keep, or `None` to keep it forever.
    pub keep_days: Option<u32>,
    /// Where the nightly copy goes. It belongs on a disk that is backed up.
    pub backup: PathBuf,
}

/// A stretch of time to read from accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The start.
    pub from: UnixTime,
    /// The end.
    pub to: UnixTime,
}

/// Where a row first came from.
#[derive(Clone, Copy)]
enum Origin {
    Live,
    Accounting,
}

/// Whether accounting covers everything up to the live samples.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Coverage {
    /// No listing yet in this run.
    Unchecked,
    /// Each jobs tick extends the coverage.
    Current,
    /// Listings stopped for a while, so accounting must fill the gap first.
    Behind,
}

/// The SQLite store. Only one process should write to it.
pub struct Store {
    db: Connection,
    path: PathBuf,
    settings: Settings,
    /// Each node's CPUs per core, from the last node listing.
    cpus_per_core: HashMap<NodeName, u32>,
    /// Each node's last recorded health and reason.
    health: HashMap<NodeName, (String, Option<String>)>,
    coverage: Coverage,
    /// Color slots below this have come out of [`Store::new_users`].
    users_seen: u32,
}

impl Store {
    /// Opens or creates the store at `path` and brings its schema up to date.
    /// Warns on stderr when `path` is on a network filesystem, where SQLite's locks are unreliable.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be opened or a migration fails.
    pub fn open(path: &Path, settings: Settings) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            if let Some(kind) = network_filesystem(parent) {
                eprintln!(
                    "warning: {} is on {kind}, but SQLite needs a local disk",
                    path.display()
                );
            }
        }
        let mut db =
            Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut db)?;
        let health = db
            .prepare(
                "SELECT node, state, reason FROM node_states AS latest
                 WHERE at = (SELECT max(at) FROM node_states WHERE node = latest.node)",
            )?
            .query_map([], |row| {
                Ok((NodeName(row.get(0)?), (row.get(1)?, row.get(2)?)))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self {
            db,
            path: path.to_owned(),
            settings,
            cpus_per_core: HashMap::new(),
            health,
            coverage: Coverage::Unchecked,
            users_seen: 0,
        })
    }

    /// Writes one tick's events.
    ///
    /// # Errors
    ///
    /// Fails when a write fails.
    pub fn record(&mut self, at: UnixTime, events: &[Event]) -> Result<()> {
        let tx = self.db.transaction()?;
        for event in events {
            match event {
                Event::Started { attempt } | Event::Withdrawn { attempt } => {
                    upsert(&tx, &self.cpus_per_core, attempt, Origin::Live)?;
                }
                Event::Queued(attempts) => {
                    for attempt in attempts {
                        upsert(&tx, &self.cpus_per_core, attempt, Origin::Live)?;
                    }
                }
                Event::Ended { key, at, state } => {
                    tx.execute(
                        "UPDATE attempts SET end = ?3, state = ?4 WHERE job = ?1 AND submit = ?2",
                        params![key.job.0, key.submit.0, at.0, state.map(state_name)],
                    )?;
                }
                Event::Settled { key, state } => {
                    tx.execute(
                        "UPDATE attempts SET state = ?3 WHERE job = ?1 AND submit = ?2",
                        params![key.job.0, key.submit.0, state_name(*state)],
                    )?;
                }
                Event::Placed { key, placement } => {
                    tx.execute(
                        "UPDATE attempts SET placement = ?3, exact = ?4 WHERE job = ?1 AND submit = ?2",
                        params![
                            key.job.0,
                            key.submit.0,
                            serde_json::to_string(placement)?,
                            placement.is_exact()
                        ],
                    )?;
                }
                Event::Nodes(nodes) => {
                    self.cpus_per_core = nodes
                        .iter()
                        .map(|node| (node.name.clone(), node.cpus_per_core()))
                        .collect();
                    for node in nodes {
                        let health = (health(node).to_owned(), node.reason.clone());
                        if self.health.get(&node.name) != Some(&health) {
                            tx.execute(
                                "INSERT OR REPLACE INTO node_states VALUES (?1, ?2, ?3, ?4)",
                                params![node.name.0, at.0, health.0, health.1],
                            )?;
                            self.health.insert(node.name.clone(), health);
                        }
                    }
                }
                Event::Dequeued(_)
                | Event::Partitions(_)
                | Event::Stats(_)
                | Event::Users(_)
                | Event::Outage(_) => {}
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Users first seen since the last call, in slot order. The first call after opening returns everyone.
    ///
    /// # Errors
    ///
    /// Fails when a read fails.
    pub fn new_users(&mut self) -> Result<Vec<User>> {
        let users: Vec<User> = self
            .db
            .prepare(
                "SELECT login, full_name, color_slot FROM users WHERE color_slot >= ?1
                 ORDER BY color_slot",
            )?
            .query_map([self.users_seen], |row| {
                Ok(User {
                    login: Login(row.get(0)?),
                    name: row.get(1)?,
                    color_slot: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        if let Some(last) = users.last() {
            self.users_seen = last.color_slot + 1;
        }
        Ok(users)
    }

    /// Stores users' full names.
    ///
    /// # Errors
    ///
    /// Fails when a write fails.
    pub fn name_users(&mut self, names: &HashMap<Login, String>) -> Result<()> {
        let tx = self.db.transaction()?;
        for (login, name) in names {
            tx.execute(
                "UPDATE users SET full_name = ?2 WHERE login = ?1",
                params![login.0, name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Notes a successful job listing at `at`. A new store starts its history and its bins there.
    ///
    /// Returns whether the listings have a gap: this is the first listing of a run on an existing store,
    /// or the last one came more than `max_gap_s` ago, as when the controller was down. Jobs can then
    /// have started and ended unseen. The caller should run [`Store::recover`], and the next backfills
    /// read the gap from accounting.
    ///
    /// # Errors
    ///
    /// Fails when a read or write fails.
    pub fn sampled(&mut self, at: UnixTime, max_gap_s: i64) -> Result<bool> {
        let Some(last) = self.meta("complete_to")? else {
            self.set_meta("created", at)?;
            self.set_meta("history_from", at)?;
            self.set_meta("rolled_to", bin_of(at))?;
            self.set_meta("complete_to", at)?;
            self.coverage = Coverage::Current;
            return Ok(false);
        };
        let gap = match self.coverage {
            Coverage::Unchecked => true,
            Coverage::Current => at.0 - last.0 > max_gap_s,
            Coverage::Behind => false,
        };
        if gap {
            self.coverage = Coverage::Behind;
        }
        if self.coverage == Coverage::Current {
            self.set_meta("complete_to", at)?;
        }
        Ok(gap)
    }

    /// Ends every open attempt that is not `active` as `lost`, when the listings stopped.
    /// Run it when [`Store::sampled`] finds a gap. A backfill of the gap then replaces these ends with
    /// the ones accounting holds.
    ///
    /// # Errors
    ///
    /// Fails when a read or write fails.
    pub fn recover(&mut self, active: &HashSet<AttemptKey>) -> Result<usize> {
        let Some(last) = self.meta("complete_to")? else {
            return Ok(0);
        };
        let tx = self.db.transaction()?;
        let open: Vec<AttemptKey> = tx
            .prepare("SELECT job, submit FROM attempts WHERE end IS NULL")?
            .query_map([], |row| {
                Ok(AttemptKey {
                    job: JobId(row.get(0)?),
                    submit: UnixTime(row.get(1)?),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut closed = 0;
        for key in open.iter().filter(|key| !active.contains(key)) {
            closed += tx.execute(
                "UPDATE attempts SET end = max(?3, coalesce(start, submit)), state = ?4
                 WHERE job = ?1 AND submit = ?2",
                params![key.job.0, key.submit.0, last.0, state_name(EndState::Lost)],
            )?;
        }
        tx.commit()?;
        Ok(closed)
    }

    /// The next stretch of accounting to read, at most `span_s` long. A gap since the last sample comes
    /// first, starting `overlap_s` early. Then history goes back a stretch at a time to the backfill horizon.
    ///
    /// # Errors
    ///
    /// Fails when a read fails.
    pub fn next_backfill(
        &self,
        now: UnixTime,
        overlap_s: i64,
        span_s: i64,
    ) -> Result<Option<Window>> {
        let (Some(created), Some(from), Some(to)) = (
            self.meta("created")?,
            self.meta("history_from")?,
            self.meta("complete_to")?,
        ) else {
            return Ok(None);
        };
        if self.coverage != Coverage::Current {
            return Ok(Some(Window {
                from: UnixTime(to.0 - overlap_s),
                to: UnixTime((to.0 + span_s).min(now.0)),
            }));
        }
        let horizon = created.0 - i64::from(self.settings.backfill_days) * DAY_S;
        Ok((from.0 > horizon).then(|| Window {
            from: UnixTime((from.0 - span_s).max(horizon)),
            to: from,
        }))
    }

    /// Writes what accounting holds for `window`, as [`Store::next_backfill`] gave it, and rolls up its bins again.
    /// A record with no end whose attempt is not `active` is a runaway that the scheduler forgot, and is skipped.
    ///
    /// # Errors
    ///
    /// Fails when a read or write fails.
    pub fn backfill(
        &mut self,
        window: Window,
        attempts: &[Attempt],
        active: &HashSet<AttemptKey>,
        now: UnixTime,
    ) -> Result<()> {
        let tx = self.db.transaction()?;
        for attempt in attempts {
            if stored_times(attempt).1.is_none() && !active.contains(&attempt.key) {
                continue;
            }
            upsert(&tx, &self.cpus_per_core, attempt, Origin::Accounting)?;
        }
        tx.commit()?;
        if self.coverage == Coverage::Current {
            self.set_meta("history_from", window.from)?;
        } else {
            self.set_meta("complete_to", window.to)?;
            if window.to >= now {
                self.coverage = Coverage::Current;
            }
        }
        let rolled_to = self.meta("rolled_to")?.unwrap_or(now);
        let to = UnixTime((bin_of(window.to).0 + BIN_S).min(rolled_to.0));
        self.roll_bins(bin_of(window.from), to, now)
    }

    /// Rolls up every bin that closed at least 30 s before `now` and has not been rolled up yet.
    ///
    /// # Errors
    ///
    /// Fails when a read or write fails.
    pub fn roll(&mut self, now: UnixTime) -> Result<()> {
        let Some(from) = self.meta("rolled_to")? else {
            return Ok(());
        };
        let to = bin_of(UnixTime(now.0 - GRACE_S));
        if to <= from {
            return Ok(());
        }
        self.roll_bins(from, to, now)?;
        self.set_meta("rolled_to", to)
    }

    /// Prunes history past the retention and starts a backup, once a day.
    ///
    /// # Errors
    ///
    /// Fails when a read or write fails. A failed backup is only reported on stderr.
    pub fn maintain(&mut self, now: UnixTime) -> Result<()> {
        if self
            .meta("maintained")?
            .is_some_and(|last| now.0 - last.0 < DAY_S)
        {
            return Ok(());
        }
        self.set_meta("maintained", now)?;
        if let Some(days) = self.settings.keep_days {
            self.prune(UnixTime(now.0 - i64::from(days) * DAY_S))?;
        }
        let (source, dest) = (self.path.clone(), self.settings.backup.clone());
        std::thread::spawn(move || {
            if let Err(error) = backup(&source, &dest) {
                eprintln!("backup to {} failed: {error:#}", dest.display());
            }
        });
        Ok(())
    }

    /// Deletes attempts that ended, and bins and node states that began, before `cutoff`.
    fn prune(&mut self, cutoff: UnixTime) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM attempts WHERE end < ?1", [cutoff.0])?;
        tx.execute("DELETE FROM usage WHERE bin < ?1", [cutoff.0])?;
        tx.execute("DELETE FROM node_states WHERE at < ?1", [cutoff.0])?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces the bins from `from` up to `to` with fresh rollups.
    fn roll_bins(&mut self, from: UnixTime, to: UnixTime, now: UnixTime) -> Result<()> {
        let tx = self.db.transaction()?;
        let spans = spans(&tx, from, to)?;
        tx.execute(
            "DELETE FROM usage WHERE bin >= ?1 AND bin < ?2",
            [from.0, to.0],
        )?;
        let mut insert =
            tx.prepare("INSERT INTO usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)")?;
        let mut bin = from;
        while bin < to {
            for ((login, partition), usage) in rollup(bin, &spans, now) {
                insert.execute(params![
                    bin.0,
                    login.0,
                    partition.0,
                    usage.core_s,
                    usage.gpu_s,
                    usage.wait_s,
                    usage.started,
                    usage.ended,
                    usage.failed
                ])?;
            }
            bin = UnixTime(bin.0 + BIN_S);
        }
        drop(insert);
        tx.commit()?;
        Ok(())
    }

    fn meta(&self, key: &str) -> Result<Option<UnixTime>> {
        meta(&self.db, key)
    }

    fn set_meta(&self, key: &str, value: UnixTime) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO meta VALUES (?1, ?2)",
            params![key, value.0],
        )?;
        Ok(())
    }
}

/// What to sum usage by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    /// Each login.
    Login,
    /// Each partition.
    Partition,
}

/// A stretch of usage to sum, step by step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsageQuery {
    /// The start, rounded down to a bin.
    pub from: UnixTime,
    /// The end. Bins that start at or after it are left out.
    pub to: UnixTime,
    /// The step, rounded up to whole bins.
    pub step_s: i64,
    /// What to sum by.
    pub by: By,
}

/// An attempt as history holds it.
#[derive(Clone, Debug, PartialEq)]
pub struct Stored {
    /// The attempt as the queue listed it while it waited. History keeps no name, reason, time limit,
    /// raw state or estimated start, and only the first partition.
    pub attempt: Attempt,
    /// When it took its cores.
    pub start: Option<UnixTime>,
    /// When it freed them, or left the queue without taking any.
    pub end: Option<UnixTime>,
    /// How it ended, once known.
    pub state: Option<EndState>,
    /// Its exact cells, when a lookup found them.
    pub placement: Option<Placement>,
}

/// A node's health from one moment until its next record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeHealth {
    /// The node.
    pub node: NodeName,
    /// When the sampler first saw this health.
    pub at: UnixTime,
    /// Draining, drained, down or unknown, or `None` while it was up. History does not split up into
    /// idle, mixed and allocated.
    pub state: Option<NodeState>,
    /// Why an operator drained it or took it down.
    pub reason: Option<String>,
}

/// Reads history on a connection of its own, so a server can query while the sampler writes.
pub struct History {
    path: PathBuf,
}

impl History {
    /// History in the store at `path`. Nothing is opened until a query runs.
    pub fn open(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    /// Usage summed per step, by login or partition, in time then key order. Bins the sampler has rolled
    /// up come from the usage table. Later bins are rolled up from the attempts, counting up to `now`.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn usage(&self, query: &UsageQuery, now: UnixTime) -> Result<Vec<Step>> {
        let db = self.connect()?;
        let Some(rolled_to) = meta(&db, "rolled_to")? else {
            return Ok(Vec::new());
        };
        let step_s = (query.step_s.max(1) + BIN_S - 1) / BIN_S * BIN_S;
        let from = bin_of(query.from);
        let to = query.to.min(now);
        let step_of = |bin: UnixTime| from.0 + (bin.0 - from.0) / step_s * step_s;
        let column = match query.by {
            By::Login => "login",
            By::Partition => "partition",
        };
        let mut sums: BTreeMap<(i64, String), Usage> = BTreeMap::new();

        let mut closed = db.prepare(&format!(
            "SELECT ?1 + (bin - ?1) / ?3 * ?3 AS at, {column}, sum(core_s), sum(gpu_s), sum(wait_s),
                    sum(started), sum(ended), sum(failed)
             FROM usage WHERE bin >= ?1 AND bin < ?2 GROUP BY at, {column}"
        ))?;
        let rows = closed.query_map([from.0, to.0.min(rolled_to.0), step_s], |row| {
            let usage = Usage {
                core_s: row.get(2)?,
                gpu_s: row.get(3)?,
                wait_s: row.get(4)?,
                started: row.get(5)?,
                ended: row.get(6)?,
                failed: row.get(7)?,
            };
            Ok(((row.get(0)?, row.get(1)?), usage))
        })?;
        for row in rows {
            let (key, usage) = row?;
            *sums.entry(key).or_default() += &usage;
        }

        let mut bin = from.max(rolled_to);
        let spans = spans(&db, bin, to)?;
        while bin < to {
            for ((login, partition), usage) in rollup(bin, &spans, now) {
                let key = match query.by {
                    By::Login => login.0,
                    By::Partition => partition.0,
                };
                *sums.entry((step_of(bin), key)).or_default() += &usage;
            }
            bin = UnixTime(bin.0 + BIN_S);
        }

        Ok(sums
            .into_iter()
            .map(|((at, key), usage)| Step {
                at: UnixTime(at),
                key,
                core_s: usage.core_s,
                gpu_s: usage.gpu_s,
                wait_s: usage.wait_s,
                started: usage.started,
                ended: usage.ended,
                failed: (query.by == By::Partition).then_some(usage.failed),
            })
            .collect())
    }

    /// When history starts: the earliest moment backfill has reached, or `None` for a new store.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn starts(&self) -> Result<Option<UnixTime>> {
        meta(&self.connect()?, "history_from")
    }

    /// Attempts submitted by `at` that had not ended by then: those running at `at` and those waiting.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn alive(&self, at: UnixTime) -> Result<Vec<Stored>> {
        // Two halves, so each uses an index: one on ends and one on open attempts.
        stored(
            &self.connect()?,
            &format!(
                "SELECT {STORED} FROM attempts WHERE end > ?1 AND submit <= ?1
                 UNION ALL
                 SELECT {STORED} FROM attempts WHERE end IS NULL AND submit <= ?1"
            ),
            [at.0],
        )
    }

    /// Attempts submitted, started or ended after `from` and up to `to`.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn touched(&self, from: UnixTime, to: UnixTime) -> Result<Vec<Stored>> {
        stored(
            &self.connect()?,
            &format!(
                "SELECT {STORED} FROM attempts WHERE submit > ?1 AND submit <= ?2
                 UNION
                 SELECT {STORED} FROM attempts WHERE start > ?1 AND start <= ?2
                 UNION
                 SELECT {STORED} FROM attempts WHERE end > ?1 AND end <= ?2"
            ),
            [from.0, to.0],
        )
    }

    /// Every recorded change of node health up to `to`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn node_health(&self, to: UnixTime) -> Result<Vec<NodeHealth>> {
        let db = self.connect()?;
        let mut query = db.prepare(
            "SELECT node, at, state, reason FROM node_states WHERE at <= ?1 ORDER BY at, node",
        )?;
        let rows = query.query_map([to.0], |row| {
            Ok(NodeHealth {
                node: NodeName(row.get(0)?),
                at: UnixTime(row.get(1)?),
                state: health_state(&row.get::<_, String>(2)?),
                reason: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn connect(&self) -> Result<Connection> {
        Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("cannot open {}", self.path.display()))
    }
}

fn meta(db: &Connection, key: &str) -> Result<Option<UnixTime>> {
    Ok(db
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0).map(UnixTime)
        })
        .optional()?)
}

fn migrate(db: &mut Connection) -> Result<()> {
    let done: usize = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let tx = db.transaction()?;
    for (version, migration) in MIGRATIONS.iter().enumerate().skip(done) {
        tx.execute_batch(migration)
            .with_context(|| format!("migration {}", version + 1))?;
        tx.pragma_update(None, "user_version", version + 1)?;
    }
    tx.commit()?;
    Ok(())
}

/// The start and end to store. A pending attempt's start is only an estimate, and a running one's end
/// only its time limit, so neither is kept.
fn stored_times(attempt: &Attempt) -> (Option<UnixTime>, Option<UnixTime>) {
    match attempt.state {
        JobState::Pending => (None, None),
        JobState::Running | JobState::Suspended => (attempt.start, None),
        JobState::Completing | JobState::Ended(_) => (attempt.start, attempt.end),
    }
}

/// Inserts an attempt, or updates it while keeping its placement and origin.
/// A start, end or state already stored survives an update that lacks one.
fn upsert(
    tx: &Transaction,
    cpus_per_core: &HashMap<NodeName, u32>,
    attempt: &Attempt,
    origin: Origin,
) -> Result<()> {
    let (start, end) = stored_times(attempt);
    let state = match attempt.state {
        JobState::Ended(state) => Some(state_name(state)),
        _ => None,
    };
    let cores = attempt.nodes.first().map(|node| {
        let per_core = cpus_per_core
            .get(node)
            .copied()
            .unwrap_or_else(|| usual(cpus_per_core));
        attempt.cpus.div_ceil(per_core)
    });
    let origin = match origin {
        Origin::Live => "live",
        Origin::Accounting => "accounting",
    };
    let partition = attempt
        .partitions
        .first()
        .map_or("", |name| name.0.as_str());
    tx.prepare_cached(
        "INSERT INTO attempts
             (job, submit, array_job, task_range, tasks, login, partition, cpus, cores, gpus, mem_mb,
              start, end, state, nodes, origin)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
         ON CONFLICT (job, submit) DO UPDATE SET
             array_job = excluded.array_job, task_range = excluded.task_range, tasks = excluded.tasks,
             login = excluded.login, partition = excluded.partition, cpus = excluded.cpus,
             cores = coalesce(excluded.cores, cores), gpus = excluded.gpus, mem_mb = excluded.mem_mb,
             start = coalesce(excluded.start, start), end = coalesce(excluded.end, end),
             state = coalesce(excluded.state, state),
             nodes = iif(excluded.nodes = '[]', nodes, excluded.nodes)",
    )?
    .execute(params![
        attempt.key.job.0,
        attempt.key.submit.0,
        attempt.array.as_ref().map(|array| array.job.0),
        attempt.array.as_ref().map(|array| &array.spec),
        attempt.array.as_ref().map_or(1, |array| array.count),
        attempt.login.0,
        partition,
        attempt.cpus,
        cores,
        attempt.gpus,
        attempt.mem_mb,
        start.map(|time| time.0),
        end.map(|time| time.0),
        state,
        serde_json::to_string(&attempt.nodes)?,
        origin
    ])?;
    tx.prepare_cached(
        "INSERT OR IGNORE INTO users (login, color_slot) VALUES (?1, (SELECT count(*) FROM users))",
    )?
    .execute([&attempt.login.0])?;
    Ok(())
}

/// The CPUs per core most nodes have, for a node that has left the listing.
fn usual(cpus_per_core: &HashMap<NodeName, u32>) -> u32 {
    let mut counts: HashMap<u32, usize> = HashMap::new();
    for per_core in cpus_per_core.values() {
        *counts.entry(*per_core).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(per_core, count)| (count, per_core))
        .map_or(1, |(per_core, _)| per_core)
}

/// Attempts that waited or ran at some point from `from` up to `to`.
fn spans(db: &Connection, from: UnixTime, to: UnixTime) -> Result<Vec<Span>> {
    let mut query = db.prepare(
        "SELECT login, partition, tasks, coalesce(cores, 0), gpus, submit, start, end, state
         FROM attempts WHERE submit < ?2 AND (end IS NULL OR end > ?1)",
    )?;
    let rows = query.query_map([from.0, to.0], |row| {
        Ok(Span {
            login: Login(row.get(0)?),
            partition: PartitionName(row.get(1)?),
            tasks: row.get(2)?,
            cores: row.get(3)?,
            gpus: row.get(4)?,
            submit: UnixTime(row.get(5)?),
            start: row.get::<_, Option<i64>>(6)?.map(UnixTime),
            end: row.get::<_, Option<i64>>(7)?.map(UnixTime),
            state: row
                .get::<_, Option<String>>(8)?
                .and_then(|name| state_from_name(&name)),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The columns [`stored`] reads, in its order.
const STORED: &str =
    "job, submit, array_job, task_range, tasks, login, partition, cpus, gpus, mem_mb,
                      start, end, state, nodes, placement";

/// Runs `sql`, which selects [`STORED`], and reads each row.
fn stored(db: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Stored>> {
    let mut query = db.prepare(sql)?;
    let rows = query.query_map(params, |row| {
        let job = JobId(row.get(0)?);
        let partition: String = row.get(6)?;
        let attempt = Attempt {
            key: AttemptKey {
                job,
                submit: UnixTime(row.get(1)?),
            },
            array: row
                .get::<_, Option<u32>>(2)?
                .map(|array| -> rusqlite::Result<ArrayTasks> {
                    Ok(ArrayTasks {
                        job: JobId(array),
                        spec: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        count: row.get(4)?,
                    })
                })
                .transpose()?,
            login: Login(row.get(5)?),
            partitions: (!partition.is_empty())
                .then_some(PartitionName(partition))
                .into_iter()
                .collect(),
            state: JobState::Pending,
            state_raw: "PENDING".to_owned(),
            cpus: row.get(7)?,
            mem_mb: row.get(9)?,
            gpus: row.get(8)?,
            time_limit_s: None,
            start: None,
            end: None,
            nodes: json(13, &row.get::<_, String>(13)?)?,
            reason: None,
            name: String::new(),
        };
        Ok(Stored {
            attempt,
            start: row.get::<_, Option<i64>>(10)?.map(UnixTime),
            end: row.get::<_, Option<i64>>(11)?.map(UnixTime),
            state: row
                .get::<_, Option<String>>(12)?
                .and_then(|name| state_from_name(&name)),
            placement: row
                .get::<_, Option<String>>(14)?
                .map(|text| json(14, &text))
                .transpose()?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Reads the JSON text in column `index`.
fn json<T: serde::de::DeserializeOwned>(index: usize, text: &str) -> rusqlite::Result<T> {
    serde_json::from_str(text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, error.into())
    })
}

/// An end state's name, as it is serialized.
fn state_name(state: EndState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn state_from_name(name: &str) -> Option<EndState> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()
}

/// A node's health for the history: allocation changes all count as `up`.
fn health(node: &Node) -> &'static str {
    match node.state {
        NodeState::Idle | NodeState::Mixed | NodeState::Allocated => "up",
        NodeState::Draining => "draining",
        NodeState::Drained => "drained",
        NodeState::Down => "down",
        NodeState::Unknown => "unknown",
    }
}

/// The node state a health that [`health`] wrote stands for, or `None` for `up`.
fn health_state(name: &str) -> Option<NodeState> {
    match name {
        "up" => None,
        "draining" => Some(NodeState::Draining),
        "drained" => Some(NodeState::Drained),
        "down" => Some(NodeState::Down),
        _ => Some(NodeState::Unknown),
    }
}

/// Copies the store at `source` to `dest` in one consistent snapshot. Writers carry on meanwhile.
fn backup(source: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = dest.with_extension("partial");
    if partial.exists() {
        fs::remove_file(&partial)?;
    }
    Connection::open(source)?.execute("VACUUM INTO ?1", [partial.to_string_lossy()])?;
    fs::rename(&partial, dest)?;
    Ok(())
}

/// The network filesystem that holds `path`, if any.
fn network_filesystem(path: &Path) -> Option<String> {
    let mounts = fs::read_to_string("/proc/self/mounts").ok()?;
    let kind = mount_kind(&path.canonicalize().ok()?, &mounts)?;
    let network = [
        "nfs",
        "nfs4",
        "ceph",
        "fuse.ceph-fuse",
        "lustre",
        "gpfs",
        "beegfs",
        "cifs",
        "smb3",
    ];
    network.contains(&kind).then(|| kind.to_owned())
}

/// The filesystem type of the longest mount in a `/proc/self/mounts` table that holds `path`.
fn mount_kind<'a>(path: &Path, mounts: &'a str) -> Option<&'a str> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().skip(1);
            Some((Path::new(fields.next()?), fields.next()?))
        })
        .filter(|(mount, _)| path.starts_with(mount))
        .max_by_key(|(mount, _)| mount.components().count())
        .map(|(_, kind)| kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtviz_model::NodeCells;

    const T0: i64 = 1_794_614_400;

    fn settings(dir: &Path) -> Settings {
        Settings {
            backfill_days: 2,
            keep_days: None,
            backup: dir.join("backup/rtviz.sqlite"),
        }
    }

    fn open(dir: &tempfile::TempDir) -> Store {
        Store::open(&dir.path().join("rtviz.sqlite"), settings(dir.path())).unwrap()
    }

    fn key(id: u32) -> AttemptKey {
        AttemptKey {
            job: JobId(id),
            submit: UnixTime(T0 - 600),
        }
    }

    fn attempt(id: u32, state: JobState, start: Option<i64>, end: Option<i64>) -> Attempt {
        Attempt {
            key: key(id),
            array: None,
            login: Login("otter".into()),
            partitions: vec![PartitionName("4hours".into()), PartitionName("long".into())],
            state,
            state_raw: String::new(),
            cpus: 8,
            mem_mb: Some(1024),
            gpus: 0,
            time_limit_s: None,
            start: start.map(UnixTime),
            end: end.map(UnixTime),
            nodes: if start.is_some() {
                vec![NodeName("z1".into())]
            } else {
                Vec::new()
            },
            reason: None,
            name: String::new(),
        }
    }

    fn node(state: NodeState) -> Node {
        Node {
            name: NodeName("z1".into()),
            partitions: Vec::new(),
            state,
            flags: Vec::new(),
            state_raw: String::new(),
            reason: None,
            cpus: 64,
            cpus_alloc: 0,
            sockets: 2,
            cores: 32,
            load: None,
            mem_mb: 0,
            mem_alloc_mb: 0,
            mem_free_mb: None,
            gpus: 0,
            gpus_used: 0,
            gpu_model: None,
            features: Vec::new(),
        }
    }

    /// `(start, end, state, cores, tasks, origin, placed)` for one attempt.
    type Row = (
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<u32>,
        u32,
        String,
        bool,
    );

    fn row(store: &Store, id: u32) -> Row {
        store
            .db
            .query_row(
                "SELECT start, end, state, cores, tasks, origin, placement IS NOT NULL
                 FROM attempts WHERE job = ?1",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap()
    }

    fn running(id: u32) -> Attempt {
        attempt(id, JobState::Running, Some(T0 - 300), Some(T0 + 3_600))
    }

    #[test]
    fn events_build_up_each_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        let mut range = attempt(1, JobState::Pending, Some(T0 + 900), None);
        range.nodes.clear();
        range.array = Some(ArrayTasks {
            job: JobId(1),
            spec: "0-3".into(),
            count: 4,
        });
        let mut cancelled = attempt(3, JobState::Ended(EndState::Cancelled), None, Some(T0));
        cancelled.nodes.clear();
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Nodes(vec![node(NodeState::Mixed)]),
                    Event::Started {
                        attempt: running(2),
                    },
                    Event::Withdrawn { attempt: cancelled },
                    Event::Queued(vec![range]),
                ],
            )
            .unwrap();
        assert_eq!(
            row(&store, 1),
            (None, None, None, None, 4, "live".into(), false),
            "a pending start is only an estimate"
        );
        assert_eq!(
            row(&store, 2),
            (Some(T0 - 300), None, None, Some(4), 1, "live".into(), false),
            "a running end is only the time limit, and 8 CPUs are 4 cores"
        );
        assert_eq!(row(&store, 3).1, Some(T0));
        assert_eq!(row(&store, 3).2.as_deref(), Some("cancelled"));

        let placement = Placement(vec![NodeCells {
            node: NodeName("z1".into()),
            cores: vec![0, 1, 2, 3],
            unplaced: 0,
            gpus: Vec::new(),
            mem_mb: None,
        }]);
        store
            .record(
                UnixTime(T0 + 10),
                &[
                    Event::Placed {
                        key: key(2),
                        placement,
                    },
                    Event::Ended {
                        key: key(2),
                        at: UnixTime(T0 + 5),
                        state: None,
                    },
                ],
            )
            .unwrap();
        assert_eq!(row(&store, 2).1, Some(T0 + 5));
        assert_eq!(row(&store, 2).2, None);
        assert!(row(&store, 2).6);
        store
            .record(
                UnixTime(T0 + 20),
                &[Event::Settled {
                    key: key(2),
                    state: EndState::OutOfMemory,
                }],
            )
            .unwrap();
        assert_eq!(row(&store, 2).2.as_deref(), Some("out_of_memory"));
        let slots: u32 = store
            .db
            .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
            .unwrap();
        assert_eq!(slots, 1);
    }

    #[test]
    fn new_users_come_out_once_and_everyone_comes_out_after_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        let heron = Attempt {
            login: Login("heron".into()),
            ..running(2)
        };
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Started {
                        attempt: running(1),
                    },
                    Event::Started { attempt: heron },
                ],
            )
            .unwrap();
        let user = |login: &str, name: Option<&str>, color_slot| User {
            login: Login(login.into()),
            name: name.map(str::to_owned),
            color_slot,
        };
        assert_eq!(
            store.new_users().unwrap(),
            [user("otter", None, 0), user("heron", None, 1)]
        );
        assert_eq!(store.new_users().unwrap(), []);
        store
            .name_users(&HashMap::from([(
                Login("heron".into()),
                "Grey Heron".to_owned(),
            )]))
            .unwrap();
        drop(store);
        assert_eq!(
            open(&dir).new_users().unwrap(),
            [user("otter", None, 0), user("heron", Some("Grey Heron"), 1)]
        );
    }

    #[test]
    fn node_health_is_written_on_change_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        for (at, state) in [
            (0, NodeState::Idle),
            (1, NodeState::Mixed),
            (2, NodeState::Draining),
        ] {
            store
                .record(UnixTime(at), &[Event::Nodes(vec![node(state)])])
                .unwrap();
        }
        drop(store);
        let mut store = open(&dir);
        store
            .record(
                UnixTime(3),
                &[Event::Nodes(vec![node(NodeState::Draining)])],
            )
            .unwrap();
        let states: Vec<(i64, String)> = store
            .db
            .prepare("SELECT at, state FROM node_states ORDER BY at")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(states, [(0, "up".into()), (2, "draining".into())]);
    }

    #[test]
    fn a_new_store_backfills_a_stretch_at_a_time_to_the_horizon() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        let now = UnixTime(T0);
        assert_eq!(store.next_backfill(now, 300, DAY_S).unwrap(), None);
        store.sampled(now, 300).unwrap();
        let mut windows = Vec::new();
        while let Some(window) = store.next_backfill(now, 300, DAY_S).unwrap() {
            store.backfill(window, &[], &HashSet::new(), now).unwrap();
            windows.push((window.from.0 - T0, window.to.0 - T0));
        }
        assert_eq!(windows, [(-DAY_S, 0), (-2 * DAY_S, -DAY_S)]);
    }

    #[test]
    fn a_restart_closes_vanished_attempts_until_accounting_gives_their_ends() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Nodes(vec![node(NodeState::Mixed)]),
                    Event::Started {
                        attempt: running(1),
                    },
                    Event::Started {
                        attempt: running(2),
                    },
                    Event::Started {
                        attempt: running(3),
                    },
                ],
            )
            .unwrap();
        store.sampled(UnixTime(T0), 300).unwrap();
        while let Some(window) = store.next_backfill(UnixTime(T0), 300, DAY_S).unwrap() {
            store
                .backfill(window, &[], &HashSet::new(), UnixTime(T0))
                .unwrap();
        }
        drop(store);

        let restart = UnixTime(T0 + 900);
        let mut store = open(&dir);
        store
            .record(
                restart,
                &[
                    Event::Nodes(vec![node(NodeState::Mixed)]),
                    Event::Started {
                        attempt: running(1),
                    },
                ],
            )
            .unwrap();
        assert!(store.sampled(restart, 300).unwrap(), "a restart is a gap");
        let active = HashSet::from([key(1)]);
        assert_eq!(store.recover(&active).unwrap(), 2);
        assert_eq!(row(&store, 2).1, Some(T0));
        assert_eq!(row(&store, 2).2.as_deref(), Some("lost"));

        let gap = store.next_backfill(restart, 300, DAY_S).unwrap().unwrap();
        assert_eq!((gap.from.0, gap.to), (T0 - 300, restart));
        let timeout = attempt(
            2,
            JobState::Ended(EndState::Timeout),
            Some(T0 - 300),
            Some(T0 + 200),
        );
        let runaway = attempt(9, JobState::Running, Some(T0 - 9_000), None);
        store
            .backfill(gap, &[timeout, runaway, running(1)], &active, restart)
            .unwrap();
        assert_eq!(
            row(&store, 2),
            (
                Some(T0 - 300),
                Some(T0 + 200),
                Some("timeout".into()),
                Some(4),
                1,
                "live".into(),
                false
            )
        );
        assert_eq!(row(&store, 3).2.as_deref(), Some("lost"));
        assert_eq!(row(&store, 1).1, None, "still running");
        let runaways: u32 = store
            .db
            .query_row("SELECT count(*) FROM attempts WHERE job = 9", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(runaways, 0);
        assert_eq!(
            store.next_backfill(restart, 300, DAY_S).unwrap(),
            None,
            "caught up, and the history already reaches the horizon"
        );
        store.sampled(UnixTime(restart.0 + 10), 300).unwrap();
        assert_eq!(
            store.meta("complete_to").unwrap(),
            Some(UnixTime(restart.0 + 10))
        );
    }

    #[test]
    fn a_controller_outage_longer_than_min_job_age_reopens_the_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Nodes(vec![node(NodeState::Mixed)]),
                    Event::Started {
                        attempt: running(1),
                    },
                    Event::Started {
                        attempt: running(2),
                    },
                ],
            )
            .unwrap();
        assert!(!store.sampled(UnixTime(T0), 300).unwrap());
        while let Some(window) = store.next_backfill(UnixTime(T0), 300, DAY_S).unwrap() {
            store
                .backfill(window, &[], &HashSet::new(), UnixTime(T0))
                .unwrap();
        }
        assert!(!store.sampled(UnixTime(T0 + 290), 300).unwrap());

        let back = UnixTime(T0 + 290 + 3 * 3_600);
        assert!(store.sampled(back, 300).unwrap(), "squeue failed for 3 h");
        assert_eq!(store.recover(&HashSet::from([key(1)])).unwrap(), 1);
        assert_eq!(row(&store, 2).1, Some(T0 + 290));
        let gap = store.next_backfill(back, 300, DAY_S).unwrap().unwrap();
        assert_eq!((gap.from.0, gap.to), (T0 - 10, back));
        let later = UnixTime(back.0 + 10);
        assert!(!store.sampled(later, 300).unwrap(), "already behind");
        assert_eq!(store.meta("complete_to").unwrap(), Some(UnixTime(T0 + 290)));

        store.backfill(gap, &[], &HashSet::new(), back).unwrap();
        assert_eq!(store.next_backfill(later, 300, DAY_S).unwrap(), None);
        assert!(!store.sampled(UnixTime(later.0 + 10), 300).unwrap());
        assert_eq!(
            store.meta("complete_to").unwrap(),
            Some(UnixTime(later.0 + 10))
        );
    }

    fn core_s(store: &Store) -> i64 {
        store
            .db
            .query_row("SELECT coalesce(sum(core_s), 0) FROM usage", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn bins_roll_up_after_the_grace_and_hold_every_core_second() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        let ended = attempt(
            1,
            JobState::Ended(EndState::Completed),
            Some(T0 + 10),
            Some(T0 + 700),
        );
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Nodes(vec![node(NodeState::Mixed)]),
                    Event::Started { attempt: ended },
                ],
            )
            .unwrap();
        store.sampled(UnixTime(T0), 300).unwrap();
        store.roll(UnixTime(T0 + 300 + GRACE_S - 1)).unwrap();
        assert_eq!(core_s(&store), 0, "the first bin is still in its grace");
        store.roll(UnixTime(T0 + 900 + GRACE_S)).unwrap();
        assert_eq!(core_s(&store), 4 * 690);
        let starts_and_ends: i64 = store
            .db
            .query_row("SELECT sum(started) + sum(ended) FROM usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(starts_and_ends, 2);
    }

    #[test]
    fn usage_sums_closed_bins_by_step_and_rolls_up_open_ones_on_the_spot() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        let failed = Attempt {
            login: Login("heron".into()),
            partitions: vec![PartitionName("long".into())],
            ..attempt(
                2,
                JobState::Ended(EndState::Failed),
                Some(T0),
                Some(T0 + 600),
            )
        };
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Nodes(vec![node(NodeState::Mixed)]),
                    Event::Started {
                        attempt: running(1),
                    },
                    Event::Started { attempt: failed },
                ],
            )
            .unwrap();
        store.sampled(UnixTime(T0), 300).unwrap();
        store.roll(UnixTime(T0 + 900 + GRACE_S)).unwrap();

        let history = History::open(&dir.path().join("rtviz.sqlite"));
        let usage = |by| {
            let query = UsageQuery {
                from: UnixTime(T0),
                to: UnixTime(T0 + 1_200),
                step_s: 600,
                by,
            };
            history
                .usage(&query, UnixTime(T0 + 1_000))
                .unwrap()
                .into_iter()
                .map(|step| {
                    (
                        step.at.0 - T0,
                        step.key,
                        step.core_s,
                        step.ended,
                        step.failed,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            usage(By::Login),
            [
                (0, "heron".into(), 2_400, 0, None),
                (0, "otter".into(), 2_400, 0, None),
                (600, "heron".into(), 0, 1, None),
                (600, "otter".into(), 1_600, 0, None),
            ],
            "otter's last 100 s come from the open bin"
        );
        assert_eq!(
            usage(By::Partition),
            [
                (0, "4hours".into(), 2_400, 0, Some(0)),
                (0, "long".into(), 2_400, 0, Some(0)),
                (600, "4hours".into(), 1_600, 0, Some(0)),
                (600, "long".into(), 0, 1, Some(1)),
            ]
        );
    }

    #[test]
    fn prune_keeps_open_attempts_and_recent_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        let old = attempt(
            1,
            JobState::Ended(EndState::Completed),
            Some(T0 - 3 * DAY_S),
            Some(T0 - 2 * DAY_S),
        );
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Started { attempt: old },
                    Event::Started {
                        attempt: running(2),
                    },
                ],
            )
            .unwrap();
        store.prune(UnixTime(T0 - DAY_S)).unwrap();
        let left: Vec<u32> = store
            .db
            .prepare("SELECT job FROM attempts")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(left, [2]);
    }

    #[test]
    fn backup_is_a_readable_copy() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(&dir);
        store
            .record(
                UnixTime(T0),
                &[Event::Started {
                    attempt: running(1),
                }],
            )
            .unwrap();
        let dest = dir.path().join("backup/copy.sqlite");
        backup(&store.path, &dest).unwrap();
        backup(&store.path, &dest).unwrap();
        let copied: u32 = Connection::open(&dest)
            .unwrap()
            .query_row("SELECT count(*) FROM attempts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(copied, 1);
    }

    #[test]
    fn the_longest_mount_decides_the_filesystem() {
        let mounts = "\
/dev/sda1 / ext4 rw 0 0
10.0.0.1:/ /home ceph rw 0 0
/dev/sdb1 /scratch ext4 rw 0 0
";
        assert_eq!(
            mount_kind(Path::new("/home/otter/rtviz"), mounts),
            Some("ceph")
        );
        assert_eq!(
            mount_kind(Path::new("/scratch/otter"), mounts),
            Some("ext4")
        );
        assert_eq!(mount_kind(Path::new("/homework"), mounts), Some("ext4"));
    }
}
