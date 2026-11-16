//! History on disk: every attempt, five-minute usage bins, node health changes and user colors.

pub mod connection;
pub mod timescale;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::rollup::{BIN_S, Span, bin_of, rollup};
use connection::{Connection, Location, Value, params};
use glurmo_model::usage::Step;
use glurmo_model::{
    ArrayTasks, Attempt, AttemptKey, EndState, Event, JobId, JobState, Login, Node, NodeFlag,
    NodeName, NodeState, PartitionName, Placement, UnixTime, User,
};
use timescale::Timescale;

const DAY_S: i64 = 86_400;

/// How long after a bin closes the live path rolls it up, so ends reported a tick late still land in it.
const GRACE_S: i64 = 30;

/// SQLite's schema changes, applied in order. `PRAGMA user_version` counts how many have run.
/// A change to the schema goes into both this list and [`TIMESCALE_MIGRATIONS`].
const SQLITE_MIGRATIONS: [&str; 3] = [
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
    // Ended attempts with a start and no nodes never ran, and old bugs stored a start for them. Their
    // bins are rolled up again by `Store::open`.
    "
    INSERT OR REPLACE INTO meta SELECT 'stale_from', min(submit) FROM attempts
        WHERE start IS NOT NULL AND end IS NOT NULL AND nodes = '[]' HAVING count(*) > 0;
    UPDATE attempts SET start = NULL WHERE start IS NOT NULL AND end IS NOT NULL AND nodes = '[]';
",
];

/// TimescaleDB's schema changes, applied in order. The `schema` key in `meta` counts how many have run.
/// The tables are SQLite's, with `attempts` and `usage` as hypertables in 30-day chunks.
const TIMESCALE_MIGRATIONS: [&str; 1] = [r#"
    CREATE EXTENSION IF NOT EXISTS timescaledb;
    CREATE TABLE attempts (
        job BIGINT NOT NULL,
        submit BIGINT NOT NULL,
        array_job BIGINT,
        task_range TEXT,
        tasks BIGINT NOT NULL,
        login TEXT NOT NULL,
        partition TEXT NOT NULL,
        cpus BIGINT NOT NULL,
        cores BIGINT,
        gpus BIGINT NOT NULL,
        mem_mb BIGINT,
        start BIGINT,
        "end" BIGINT,
        state TEXT,
        nodes TEXT NOT NULL,
        placement TEXT,
        exact BIGINT,
        origin TEXT NOT NULL,
        PRIMARY KEY (job, submit)
    );
    SELECT create_hypertable('attempts', by_range('submit', 2592000),
                             create_default_indexes => false);
    CREATE INDEX attempts_by_end ON attempts ("end");
    CREATE INDEX open_attempts ON attempts (submit) WHERE "end" IS NULL;
    CREATE INDEX attempts_by_start ON attempts (start);
    CREATE INDEX attempts_by_submit ON attempts (submit);
    CREATE TABLE usage (
        bin BIGINT NOT NULL,
        login TEXT NOT NULL,
        partition TEXT NOT NULL,
        core_s BIGINT NOT NULL,
        gpu_s BIGINT NOT NULL,
        wait_s BIGINT NOT NULL,
        started BIGINT NOT NULL,
        ended BIGINT NOT NULL,
        failed BIGINT NOT NULL,
        PRIMARY KEY (bin, login, partition)
    );
    SELECT create_hypertable('usage', by_range('bin', 2592000), create_default_indexes => false);
    CREATE TABLE node_states (
        node TEXT NOT NULL,
        at BIGINT NOT NULL,
        state TEXT NOT NULL,
        reason TEXT,
        PRIMARY KEY (node, at)
    );
    CREATE TABLE users (login TEXT PRIMARY KEY, full_name TEXT, color_slot BIGINT NOT NULL);
    CREATE FUNCTION unix_now() RETURNS BIGINT LANGUAGE SQL STABLE
        AS 'SELECT extract(epoch FROM now())::bigint';
    SELECT set_integer_now_func('attempts', 'unix_now');
    SELECT set_integer_now_func('usage', 'unix_now');
"#];

/// Compresses chunks once they are 7 days old. It runs with the first TimescaleDB migration, on a
/// server whose license allows compression. The columns to segment by are named, because
/// TimescaleDB's own choice for `attempts` would be `job`, one segment per attempt. Each compressed
/// batch of attempts keeps the range of its starts and ends, so a read of a stretch of time skips
/// the batches outside it. TimescaleDB's own choice, a bloom filter, only answers equality.
const TIMESCALE_COMPRESSION: &str = r#"
    ALTER TABLE attempts SET (timescaledb.compress, timescaledb.compress_segmentby = '',
                              timescaledb.compress_orderby = 'submit, job',
                              timescaledb.sparse_index = 'minmax(start), minmax("end")');
    SELECT add_compression_policy('attempts', compress_after => 604800);
    ALTER TABLE usage SET (timescaledb.compress, timescaledb.compress_segmentby = 'login, partition',
                           timescaledb.compress_orderby = 'bin');
    SELECT add_compression_policy('usage', compress_after => 604800);
"#;

/// The store's tables and their columns, in the order a copy fills them. `attempts` and `usage` also
/// name the column whose days split their copy.
const TABLES: [(&str, &str, Option<&str>); 5] = [
    ("users", "login, full_name, color_slot", None),
    ("node_states", "node, at, state, reason", None),
    ("meta", "key, value", None),
    (
        "attempts",
        r#"job, submit, array_job, task_range, tasks, login, partition, cpus, cores, gpus, mem_mb,
           start, "end", state, nodes, placement, exact, origin"#,
        Some("submit"),
    ),
    (
        "usage",
        "bin, login, partition, core_s, gpu_s, wait_s, started, ended, failed",
        Some("bin"),
    ),
];

/// How much history to fetch and keep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Days of accounting to backfill when the store is new.
    pub backfill_days: u32,
    /// Days of history to keep, or `None` to keep it forever.
    pub keep_days: Option<u32>,
    /// Where the nightly copy of a SQLite store goes. It belongs on a disk that is backed up.
    /// A store on TimescaleDB makes no copy, and leaves backups to the server.
    pub backup: PathBuf,
}

/// A stretch of time to read from accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The start.
    pub from: UnixTime,
    /// The end.
    pub to: UnixTime,
    /// Whether it fills the gap since the last sample, rather than going back toward the horizon.
    pub gap: bool,
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
    /// Listings stopped for a while, so accounting must fill the gap up to the listing that found it.
    Behind {
        /// When listings came back.
        since: UnixTime,
    },
}

/// The store, in a SQLite file or on a TimescaleDB server. Only one process should write to it.
pub struct Store {
    db: Connection,
    location: Location,
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
    /// Opens or creates the store at `path`, brings its schema up to date, and rolls up again the bins of
    /// attempts a migration repaired. Warns on stderr when `path` is on a network filesystem, where SQLite's locks are unreliable.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be opened, or a migration or rollup fails.
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
        let mut db = rusqlite::Connection::open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        // The store runs more statements than the default 16, and each one is prepared once and kept.
        db.set_prepared_statement_cache_capacity(64);
        migrate_sqlite(&mut db)?;
        Self::resume(
            Connection::Sqlite(db),
            Location::Sqlite(path.to_owned()),
            settings,
        )
    }

    /// Connects to the store on the TimescaleDB server that `url` names, creating its tables in an
    /// empty database, brings its schema up to date, and rolls up again the bins of attempts a
    /// migration repaired.
    ///
    /// # Errors
    ///
    /// Fails when `url` does not parse, the server cannot be reached, or a migration or rollup fails.
    pub fn connect(url: &str, settings: Settings) -> Result<Self> {
        let config = timescale::parse(url)?;
        let mut db = Connection::Timescale(Timescale::connect(&config)?);
        migrate_timescale(&mut db)?;
        Self::resume(db, Location::Timescale(Box::new(config)), settings)
    }

    /// Picks up the history in `db` where the last run left it.
    fn resume(db: Connection, location: Location, settings: Settings) -> Result<Self> {
        let health = db
            .query_map(
                "SELECT node, state, reason FROM node_states AS latest
                 WHERE at = (SELECT max(at) FROM node_states WHERE node = latest.node)",
                params![],
                |row| Ok((NodeName(row.get(0)?), (row.get(1)?, row.get(2)?))),
            )?
            .into_iter()
            .collect();
        let mut store = Self {
            db,
            location,
            settings,
            cpus_per_core: HashMap::new(),
            health,
            coverage: Coverage::Unchecked,
            users_seen: 0,
        };
        store.roll_stale_bins()?;
        Ok(store)
    }

    /// Rolls up again the closed bins from where a migration changed attempts.
    fn roll_stale_bins(&mut self) -> Result<()> {
        if let (Some(stale), Some(history_from), Some(rolled_to)) = (
            self.meta("stale_from")?,
            self.meta("history_from")?,
            self.meta("rolled_to")?,
        ) {
            self.roll_bins(bin_of(stale.max(history_from)), rolled_to, rolled_to)?;
        }
        self.db
            .execute("DELETE FROM meta WHERE key = 'stale_from'", params![])?;
        Ok(())
    }

    /// Writes one tick's events, all of them or none. A tick that fails opens a gap: the next
    /// [`Store::sampled`] reports one, so accounting fills in what the tick held.
    ///
    /// # Errors
    ///
    /// Fails when a write fails.
    pub fn record(&mut self, at: UnixTime, events: &[Event]) -> Result<()> {
        let written = self.write_tick(at, events);
        if written.is_err() {
            self.coverage = Coverage::Unchecked;
        }
        written
    }

    /// Writes one tick's events in one transaction. What the store remembers of the nodes changes
    /// only once that commits, so a tick that fails is as if it never came.
    fn write_tick(&mut self, at: UnixTime, events: &[Event]) -> Result<()> {
        let mut cpus_per_core = Cow::Borrowed(&self.cpus_per_core);
        let mut health_changes = Vec::new();
        let tx = self.db.transaction()?;
        let mut uncounted_since = None;
        for event in events {
            match event {
                Event::Started { attempt } | Event::Withdrawn { attempt } => {
                    upsert(&tx, &cpus_per_core, attempt, Origin::Live)?;
                }
                Event::Queued(attempts) => {
                    for attempt in attempts {
                        upsert(&tx, &cpus_per_core, attempt, Origin::Live)?;
                    }
                }
                Event::Ended { key, at, state } => {
                    tx.execute(
                        r#"UPDATE attempts SET "end" = ?3, state = ?4 WHERE job = ?1 AND submit = ?2"#,
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
                Event::Nodes(nodes) | Event::NodesChanged(nodes) => {
                    let first_listing = cpus_per_core.is_empty();
                    cpus_per_core.to_mut().extend(
                        nodes
                            .iter()
                            .map(|node| (node.name.clone(), node.cpus_per_core())),
                    );
                    if first_listing {
                        uncounted_since = count_cores(&tx, &cpus_per_core)?;
                    }
                    for node in nodes {
                        let health = (health(node), node.reason.clone());
                        if self.health.get(&node.name) != Some(&health) {
                            tx.execute(
                                "INSERT INTO node_states VALUES (?1, ?2, ?3, ?4)
                                 ON CONFLICT (node, at) DO UPDATE
                                 SET state = excluded.state, reason = excluded.reason",
                                params![&node.name.0, at.0, &health.0, health.1.as_deref()],
                            )?;
                            health_changes.push((node.name.clone(), health));
                        }
                    }
                }
                Event::Dequeued(_)
                | Event::Updated { .. }
                | Event::Partitions(_)
                | Event::Users(_)
                | Event::Outage(_) => {}
            }
        }
        tx.commit()?;
        if let Cow::Owned(listed) = cpus_per_core {
            self.cpus_per_core = listed;
        }
        self.health.extend(health_changes);
        if let (Some(since), Some(history_from), Some(rolled_to)) = (
            uncounted_since,
            self.meta("history_from")?,
            self.meta("rolled_to")?,
        ) {
            self.roll_bins(bin_of(since.max(history_from)), rolled_to, at)?;
        }
        Ok(())
    }

    /// Users first seen since the last call, in slot order. The first call after opening returns everyone.
    ///
    /// # Errors
    ///
    /// Fails when a read fails.
    pub fn new_users(&mut self) -> Result<Vec<User>> {
        let users = self.db.query_map(
            "SELECT login, full_name, color_slot FROM users WHERE color_slot >= ?1
             ORDER BY color_slot",
            params![self.users_seen],
            |row| {
                Ok(User {
                    login: Login(row.get(0)?),
                    name: row.get(1)?,
                    color_slot: row.get(2)?,
                })
            },
        )?;
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
                params![&login.0, name],
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
            Coverage::Behind { .. } => false,
        };
        if gap {
            self.coverage = Coverage::Behind { since: at };
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
        let open = tx.query_map(
            r#"SELECT job, submit FROM attempts WHERE "end" IS NULL"#,
            params![],
            |row| {
                Ok(AttemptKey {
                    job: JobId(row.get(0)?),
                    submit: UnixTime(row.get(1)?),
                })
            },
        )?;
        let mut closed = 0;
        for key in open.iter().filter(|key| !active.contains(key)) {
            closed += tx.execute(
                r#"UPDATE attempts SET
                       "end" = CASE WHEN ?3 > coalesce(start, submit) THEN ?3
                               ELSE coalesce(start, submit) END,
                       state = ?4
                   WHERE job = ?1 AND submit = ?2"#,
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
                gap: true,
            }));
        }
        let horizon = created.0 - i64::from(self.settings.backfill_days) * DAY_S;
        Ok((from.0 > horizon).then(|| Window {
            from: UnixTime((from.0 - span_s).max(horizon)),
            to: from,
            gap: false,
        }))
    }

    /// Writes what accounting holds for `window`, as [`Store::next_backfill`] gave it, and rolls up its bins again.
    /// A record with no end whose attempt is not `active` is a runaway that the scheduler forgot, and is skipped.
    /// `now` is the time of the last listing, which may come well after the window was given. A gap
    /// window that reaches the listing that found the gap closes it, since listings have run since then.
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
        if !window.gap {
            self.set_meta("history_from", window.from)?;
        } else if let Coverage::Behind { since } = self.coverage {
            if window.to >= since {
                self.coverage = Coverage::Current;
                self.set_meta("complete_to", now)?;
            } else {
                self.set_meta("complete_to", window.to)?;
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

    /// Prunes history past the retention and starts a backup of a SQLite store, once a day.
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
        if let Location::Sqlite(source) = &self.location {
            let (source, dest) = (source.clone(), self.settings.backup.clone());
            std::thread::spawn(move || {
                if let Err(error) = backup(&source, &dest) {
                    eprintln!("backup to {} failed: {error:#}", dest.display());
                }
            });
        }
        Ok(())
    }

    /// Deletes attempts that ended and bins that began before `cutoff`, and node states that a later
    /// one replaced by then.
    fn prune(&mut self, cutoff: UnixTime) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            r#"DELETE FROM attempts WHERE "end" < ?1"#,
            params![cutoff.0],
        )?;
        tx.execute("DELETE FROM usage WHERE bin < ?1", params![cutoff.0])?;
        tx.execute(
            "DELETE FROM node_states WHERE at < ?1 AND EXISTS (
                 SELECT 1 FROM node_states AS later
                 WHERE later.node = node_states.node AND later.at > node_states.at AND later.at <= ?1
             )",
            params![cutoff.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replaces the bins from `from` up to `to` with fresh rollups, a day at a time, so each bin walks
    /// only the attempts of its day.
    fn roll_bins(&mut self, from: UnixTime, to: UnixTime, now: UnixTime) -> Result<()> {
        let mut day = from;
        while day < to {
            let next = UnixTime((day.0 + DAY_S).min(to.0));
            self.roll_day(day, next, now)?;
            day = next;
        }
        Ok(())
    }

    fn roll_day(&mut self, from: UnixTime, to: UnixTime, now: UnixTime) -> Result<()> {
        let tx = self.db.transaction()?;
        let spans = spans(&tx, from, to)?;
        tx.execute(
            "DELETE FROM usage WHERE bin >= ?1 AND bin < ?2",
            params![from.0, to.0],
        )?;
        let mut bin = from;
        while bin < to {
            for ((login, partition), usage) in rollup(bin, &spans, now) {
                tx.execute(
                    "INSERT INTO usage VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        bin.0,
                        login.0,
                        partition.0,
                        i64::try_from(usage.core_s)?,
                        i64::try_from(usage.gpu_s)?,
                        i64::try_from(usage.wait_s)?,
                        usage.started,
                        usage.ended,
                        usage.failed
                    ],
                )?;
            }
            bin = UnixTime(bin.0 + BIN_S);
        }
        tx.commit()?;
        Ok(())
    }

    /// Reads this store's history on connections of its own.
    pub fn history(&self) -> History {
        History {
            location: self.location.clone(),
        }
    }

    /// Where the store's data lives, for a log line.
    pub fn location(&self) -> &Location {
        &self.location
    }

    fn meta(&self, key: &str) -> Result<Option<UnixTime>> {
        meta(&self.db, key)
    }

    fn set_meta(&self, key: &str, value: UnixTime) -> Result<()> {
        self.db.execute(
            "INSERT INTO meta VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
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
    /// Whether the controller had lost contact with it.
    pub not_responding: bool,
    /// Why an operator drained it or took it down.
    pub reason: Option<String>,
}

/// Reads a store's history, so a server can query while the sampler writes. Each query runs on a
/// connection of its own. [`Store::history`] makes one.
pub struct History {
    location: Location,
}

impl History {
    /// Core and wait seconds summed per step, by login or partition, in time then key order. Steps with
    /// neither are left out. Bins the sampler has rolled
    /// up come from the usage table. Later bins are rolled up from the attempts, counting up to the
    /// query's end or `now`, whichever comes first.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn usage(&self, query: &UsageQuery, now: UnixTime) -> Result<Vec<Step>> {
        let db = self.location.read()?;
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
        let mut sums: BTreeMap<(i64, String), (u64, u64)> = BTreeMap::new();

        let closed: Vec<(_, (u64, u64))> = db.query_map(
            &format!(
                "SELECT ?1 + (bin - ?1) / ?3 * ?3 AS at, {column},
                        CAST(sum(core_s) AS BIGINT), CAST(sum(wait_s) AS BIGINT)
                 FROM usage WHERE bin >= ?1 AND bin < ?2 GROUP BY at, {column}"
            ),
            params![from.0, to.0.min(rolled_to.0), step_s],
            |row| Ok(((row.get(0)?, row.get(1)?), (row.get(2)?, row.get(3)?))),
        )?;
        for (key, (core_s, wait_s)) in closed {
            let sum = sums.entry(key).or_default();
            sum.0 += core_s;
            sum.1 += wait_s;
        }

        let mut bin = from.max(rolled_to);
        let spans = if bin < to {
            spans(&db, bin, to)?
        } else {
            Vec::new()
        };
        while bin < to {
            for ((login, partition), usage) in rollup(bin, &spans, to) {
                let key = match query.by {
                    By::Login => login.0,
                    By::Partition => partition.0,
                };
                let sum = sums.entry((step_of(bin), key)).or_default();
                sum.0 += usage.core_s;
                sum.1 += usage.wait_s;
            }
            bin = UnixTime(bin.0 + BIN_S);
        }

        Ok(sums
            .into_iter()
            .filter(|(_, sum)| *sum != (0, 0))
            .map(|((at, key), (core_s, wait_s))| Step {
                at: UnixTime(at),
                key,
                core_s,
                wait_s,
            })
            .collect())
    }

    /// When history starts: the earliest moment backfill has reached, or `None` for a new store.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn starts(&self) -> Result<Option<UnixTime>> {
        meta(&self.location.read()?, "history_from")
    }

    /// Attempts submitted by `at` that had not ended by then: those running at `at` and those waiting.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn alive(&self, at: UnixTime) -> Result<Vec<Stored>> {
        let db = self.location.read()?;
        let index = index_choice(&db, at, at)?;
        stored(&db, &overlapping(STORED, "<= ?1", index), params![at.0])
    }

    /// Attempts submitted, started or ended after `from` and up to `to`.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn touched(&self, from: UnixTime, to: UnixTime) -> Result<Vec<Stored>> {
        stored(
            &self.location.read()?,
            &format!(
                r#"SELECT {STORED} FROM attempts WHERE submit > ?1 AND submit <= ?2
                   UNION
                   SELECT {STORED} FROM attempts WHERE start > ?1 AND start <= ?2
                   UNION
                   SELECT {STORED} FROM attempts WHERE "end" > ?1 AND "end" <= ?2"#
            ),
            params![from.0, to.0],
        )
    }

    /// Every recorded change of node health up to `to`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the store cannot be opened or read.
    pub fn node_health(&self, to: UnixTime) -> Result<Vec<NodeHealth>> {
        self.location.read()?.query_map(
            "SELECT node, at, state, reason FROM node_states WHERE at <= ?1 ORDER BY at, node",
            params![to.0],
            |row| {
                let health: String = row.get(2)?;
                let state = health.strip_suffix('*');
                Ok(NodeHealth {
                    node: NodeName(row.get(0)?),
                    at: UnixTime(row.get(1)?),
                    state: health_state(state.unwrap_or(&health)),
                    not_responding: state.is_some(),
                    reason: row.get(3)?,
                })
            },
        )
    }
}

/// How many rows of one table each side holds after a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copied {
    /// The table.
    pub table: &'static str,
    /// Its rows in the SQLite store.
    pub source: u64,
    /// Its rows in the TimescaleDB database.
    pub target: u64,
}

/// Copies the SQLite store at `source` into the TimescaleDB database that `url` names, which must hold
/// no history yet. A store opened on the copy carries on where the SQLite one stood: its first listing
/// finds a gap, and accounting fills it.
///
/// The source is read in one transaction, so a sampler may keep writing to it. Each day of attempts
/// and of usage is written in a transaction of its own.
///
/// # Errors
///
/// Fails when the source cannot be opened or has another schema version than this glurmo's, the
/// server cannot be reached, the database already holds history, or a read or write fails.
pub fn copy(source: &Path, url: &str) -> Result<Vec<Copied>> {
    let mut source = Location::Sqlite(source.to_owned()).read()?;
    let version: usize = source.query_row("PRAGMA user_version", params![], |row| row.get(0))?;
    if version != SQLITE_MIGRATIONS.len() {
        bail!(
            "the SQLite store has schema version {version}, and this glurmo copies version {}",
            SQLITE_MIGRATIONS.len()
        );
    }
    let snapshot = source.transaction()?;
    let mut target = Connection::Timescale(Timescale::connect(&timescale::parse(url)?)?);
    migrate_timescale(&mut target)?;
    for (table, ..) in TABLES {
        if history_rows(&target, table)? > 0 {
            bail!("the TimescaleDB database already holds history in {table}");
        }
    }
    for (table, columns, day_column) in TABLES {
        let Some(day_column) = day_column else {
            let tx = target.transaction()?;
            copy_rows(&snapshot, &tx, table, columns, "")?;
            tx.commit()?;
            continue;
        };
        let (first, last): (Option<i64>, Option<i64>) = snapshot.query_row(
            &format!("SELECT min({day_column}), max({day_column}) FROM {table}"),
            params![],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let (Some(first), Some(last)) = (first, last) else {
            continue;
        };
        let mut day = first.div_euclid(DAY_S) * DAY_S;
        while day <= last {
            let next = day + DAY_S;
            let within = format!("WHERE {day_column} >= {day} AND {day_column} < {next}");
            let tx = target.transaction()?;
            copy_rows(&snapshot, &tx, table, columns, &within)?;
            tx.commit()?;
            day = next;
        }
    }
    // The planner knows nothing of the new rows until the server analyzes them, which can take a while.
    target.execute_batch("ANALYZE")?;
    TABLES
        .iter()
        .map(|&(table, ..)| {
            Ok(Copied {
                table,
                source: history_rows(&snapshot, table)?,
                target: history_rows(&target, table)?,
            })
        })
        .collect()
}

/// How many rows of history `table` holds. The `schema` key in `meta` counts migrations, and only
/// TimescaleDB has it.
fn history_rows(db: &Connection, table: &str) -> Result<u64> {
    let filter = if table == "meta" {
        "WHERE key != 'schema'"
    } else {
        ""
    };
    db.query_row(
        &format!("SELECT count(*) FROM {table} {filter}"),
        params![],
        |row| row.get(0),
    )
}

/// Copies the rows of `table` that `filter` selects, which may be empty or a `WHERE` clause.
fn copy_rows(
    from: &Connection,
    to: &Connection,
    table: &str,
    columns: &str,
    filter: &str,
) -> Result<()> {
    let width = columns.split(',').count();
    let rows: Vec<Vec<Value>> = from.query_map(
        &format!("SELECT {columns} FROM {table} {filter}"),
        params![],
        |row| (0..width).map(|index| row.get(index)).collect(),
    )?;
    let places: Vec<String> = (1..=width).map(|number| format!("?{number}")).collect();
    let insert = format!(
        "INSERT INTO {table} ({columns}) VALUES ({})",
        places.join(", ")
    );
    for row in &rows {
        to.execute(&insert, row)?;
    }
    Ok(())
}

fn meta(db: &Connection, key: &str) -> Result<Option<UnixTime>> {
    let values = db.query_map(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0).map(UnixTime),
    )?;
    Ok(values.first().copied())
}

fn migrate_sqlite(db: &mut rusqlite::Connection) -> Result<()> {
    let done: usize = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if done > SQLITE_MIGRATIONS.len() {
        bail!(
            "the store has schema version {done}, newer than this glurmo's {}",
            SQLITE_MIGRATIONS.len()
        );
    }
    let tx = db.transaction()?;
    for (version, migration) in SQLITE_MIGRATIONS.iter().enumerate().skip(done) {
        tx.execute_batch(migration)
            .with_context(|| format!("migration {}", version + 1))?;
        tx.pragma_update(None, "user_version", version + 1)?;
    }
    tx.commit()?;
    Ok(())
}

fn migrate_timescale(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value BIGINT NOT NULL)",
    )?;
    let done = tx
        .query_map(
            "SELECT value FROM meta WHERE key = 'schema'",
            params![],
            |row| row.get::<usize>(0),
        )?
        .first()
        .copied()
        .unwrap_or(0);
    if done > TIMESCALE_MIGRATIONS.len() {
        bail!(
            "the store has schema version {done}, newer than this glurmo's {}",
            TIMESCALE_MIGRATIONS.len()
        );
    }
    for (version, migration) in TIMESCALE_MIGRATIONS.iter().enumerate().skip(done) {
        tx.execute_batch(migration)
            .with_context(|| format!("migration {}", version + 1))?;
        if version == 0 {
            compress_old_chunks(&tx)?;
        }
    }
    tx.execute(
        "INSERT INTO meta VALUES ('schema', ?1)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![i64::try_from(TIMESCALE_MIGRATIONS.len())?],
    )?;
    tx.commit()
}

/// Turns compression on where the server's license covers it. The Apache-licensed build has none.
fn compress_old_chunks(db: &Connection) -> Result<()> {
    let license: String = db.query_row(
        "SELECT current_setting('timescaledb.license')",
        params![],
        |row| row.get(0),
    )?;
    if license == "timescale" {
        db.execute_batch(TIMESCALE_COMPRESSION)
    } else {
        eprintln!(
            "warning: this TimescaleDB build has the {license} license, so history stays uncompressed"
        );
        Ok(())
    }
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
    tx: &Connection,
    cpus_per_core: &HashMap<NodeName, u32>,
    attempt: &Attempt,
    origin: Origin,
) -> Result<()> {
    let (start, end) = stored_times(attempt);
    let state = match attempt.state {
        JobState::Ended(state) => Some(state_name(state)),
        _ => None,
    };
    let cores = cores(attempt.cpus, &attempt.nodes, cpus_per_core);
    let origin = match origin {
        Origin::Live => "live",
        Origin::Accounting => "accounting",
    };
    let partition = attempt
        .partitions
        .first()
        .map_or("", |name| name.0.as_str());
    tx.execute(
        r#"INSERT INTO attempts
               (job, submit, array_job, task_range, tasks, login, partition, cpus, cores, gpus, mem_mb,
                start, "end", state, nodes, origin)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
           ON CONFLICT (job, submit) DO UPDATE SET
               array_job = excluded.array_job, task_range = excluded.task_range,
               tasks = excluded.tasks, login = excluded.login, partition = excluded.partition,
               cpus = excluded.cpus, cores = coalesce(excluded.cores, attempts.cores),
               gpus = excluded.gpus, mem_mb = excluded.mem_mb,
               start = coalesce(excluded.start, attempts.start),
               "end" = coalesce(excluded."end", attempts."end"),
               state = coalesce(excluded.state, attempts.state),
               nodes = CASE WHEN excluded.nodes = '[]' THEN attempts.nodes ELSE excluded.nodes END"#,
        params![
            attempt.key.job.0,
            attempt.key.submit.0,
            attempt.array.as_ref().map(|array| array.job.0),
            attempt.array.as_ref().map(|array| &array.spec),
            attempt.array.as_ref().map_or(1, |array| array.count),
            &attempt.login.0,
            partition,
            attempt.cpus,
            cores,
            attempt.gpus,
            attempt.mem_mb.map(i64::try_from).transpose()?,
            start.map(|time| time.0),
            end.map(|time| time.0),
            state,
            serde_json::to_string(&attempt.nodes)?,
            origin
        ],
    )?;
    tx.execute(
        "INSERT INTO users (login, color_slot) VALUES (?1, (SELECT count(*) FROM users))
         ON CONFLICT (login) DO NOTHING",
        params![&attempt.login.0],
    )?;
    Ok(())
}

/// How many cores `cpus` CPUs on `nodes` take, or `None` before the attempt has nodes or the run has
/// listed any node's threads per core.
fn cores(cpus: u32, nodes: &[NodeName], cpus_per_core: &HashMap<NodeName, u32>) -> Option<u32> {
    if cpus_per_core.is_empty() {
        return None;
    }
    nodes.first().map(|node| {
        let per_core = cpus_per_core
            .get(node)
            .copied()
            .unwrap_or_else(|| usual(cpus_per_core));
        cpus.div_ceil(per_core)
    })
}

/// Counts the cores of attempts stored with nodes before the run's first node listing. Returns when
/// the earliest of them started, from where the bins need rolling up again.
fn count_cores(
    tx: &Connection,
    cpus_per_core: &HashMap<NodeName, u32>,
) -> Result<Option<UnixTime>> {
    let uncounted: Vec<(u32, i64, u32, String, i64)> = tx.query_map(
        "SELECT job, submit, cpus, nodes, coalesce(start, submit) FROM attempts
         WHERE cores IS NULL AND nodes != '[]'",
        params![],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    for (job, submit, cpus, nodes, _) in &uncounted {
        let nodes: Vec<NodeName> = serde_json::from_str(nodes)?;
        tx.execute(
            "UPDATE attempts SET cores = ?3 WHERE job = ?1 AND submit = ?2",
            params![*job, *submit, cores(*cpus, &nodes, cpus_per_core)],
        )?;
    }
    Ok(uncounted.iter().map(|&(.., start)| UnixTime(start)).min())
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

/// The index that finds the attempts alive in a stretch of time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexChoice {
    /// Attempts by end.
    ByEnd,
    /// Attempts by submit time.
    BySubmit,
    /// Whichever the database picks.
    Planner,
}

/// Picks the index that finds the attempts alive between `from` and `to`. SQLite does not weigh its
/// indexes by bound values, so it gets the one with fewer attempts to walk: by end when fewer ended
/// after `from` than were submitted before `to`. Both counts run on their own index, and the second
/// stops once it passes the first. PostgreSQL weighs them itself. A named index would also keep
/// TimescaleDB from skipping chunks by submit time and compressed batches by end.
fn index_choice(db: &Connection, from: UnixTime, to: UnixTime) -> Result<IndexChoice> {
    if let Connection::Timescale(_) = db {
        return Ok(IndexChoice::Planner);
    }
    let ended: i64 = db.query_row(
        r#"SELECT count(*) FROM attempts WHERE "end" > ?1"#,
        params![from.0],
        |row| row.get(0),
    )?;
    let submitted: i64 = db.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM attempts WHERE submit < ?1 LIMIT ?2) AS submitted",
        params![to.0, ended + 1],
        |row| row.get(0),
    )?;
    Ok(if ended < submitted {
        IndexChoice::ByEnd
    } else {
        IndexChoice::BySubmit
    })
}

/// Selects `columns` of the attempts submitted `submitted`, such as `< ?2`, that had not ended by `?1`.
/// A unary plus keeps a column's index out of the first half of the union, which leaves the index
/// that `index` names.
fn overlapping(columns: &str, submitted: &str, index: IndexChoice) -> String {
    let (end, submit) = match index {
        IndexChoice::ByEnd => (r#""end""#, "+submit"),
        IndexChoice::BySubmit => (r#"+"end""#, "submit"),
        IndexChoice::Planner => (r#""end""#, "submit"),
    };
    format!(
        r#"SELECT {columns} FROM attempts WHERE {end} > ?1 AND {submit} {submitted}
           UNION ALL
           SELECT {columns} FROM attempts WHERE "end" IS NULL AND submit {submitted}"#
    )
}

/// Attempts that waited or ran at some point from `from` up to `to`.
fn spans(db: &Connection, from: UnixTime, to: UnixTime) -> Result<Vec<Span>> {
    let sql = overlapping(
        r#"login, partition, tasks, coalesce(cores, 0), gpus, submit, start, "end", state"#,
        "< ?2",
        index_choice(db, from, to)?,
    );
    db.query_map(&sql, params![from.0, to.0], |row| {
        Ok(Span {
            login: Login(row.get(0)?),
            partition: PartitionName(row.get(1)?),
            tasks: row.get(2)?,
            cores: row.get(3)?,
            gpus: row.get(4)?,
            submit: UnixTime(row.get(5)?),
            start: row.get::<Option<i64>>(6)?.map(UnixTime),
            end: row.get::<Option<i64>>(7)?.map(UnixTime),
            state: row
                .get::<Option<String>>(8)?
                .and_then(|name| state_from_name(&name)),
        })
    })
}

/// The columns [`stored`] reads, in its order.
const STORED: &str = r#"job, submit, array_job, task_range, tasks, login, partition, cpus, gpus,
                        mem_mb, start, "end", state, nodes, placement"#;

/// Runs `sql`, which selects [`STORED`], and reads each row.
fn stored(db: &Connection, sql: &str, params: &[Value]) -> Result<Vec<Stored>> {
    db.query_map(sql, params, |row| {
        let job = JobId(row.get(0)?);
        let partition: String = row.get(6)?;
        let attempt = Attempt {
            key: AttemptKey {
                job,
                submit: UnixTime(row.get(1)?),
            },
            array: row
                .get::<Option<u32>>(2)?
                .map(|array| -> Result<ArrayTasks> {
                    Ok(ArrayTasks {
                        job: JobId(array),
                        spec: row.get::<Option<String>>(3)?.unwrap_or_default(),
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
            nodes: serde_json::from_str(&row.get::<String>(13)?)?,
            reason: None,
            name: String::new(),
        };
        Ok(Stored {
            attempt,
            start: row.get::<Option<i64>>(10)?.map(UnixTime),
            end: row.get::<Option<i64>>(11)?.map(UnixTime),
            state: row
                .get::<Option<String>>(12)?
                .and_then(|name| state_from_name(&name)),
            placement: row
                .get::<Option<String>>(14)?
                .map(|text| serde_json::from_str(&text))
                .transpose()?,
        })
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

/// A node's health for the history: allocation changes all count as `up`. A trailing `*` marks a node
/// that does not answer the controller, as sinfo prints it.
fn health(node: &Node) -> String {
    let state = match node.state {
        NodeState::Idle | NodeState::Mixed | NodeState::Allocated => "up",
        NodeState::Draining => "draining",
        NodeState::Drained => "drained",
        NodeState::Down => "down",
        NodeState::Unknown => "unknown",
    };
    if node.flags.contains(&NodeFlag::NotResponding) {
        format!("{state}*")
    } else {
        state.to_owned()
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
    rusqlite::Connection::open(source)?.execute("VACUUM INTO ?1", [partial.to_string_lossy()])?;
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
    use crate::fixture::{StoreHome, busy_store_in};
    use crate::hub::Hub;
    use glurmo_model::NodeCells;

    const T0: i64 = 1_794_614_400;

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
                r#"SELECT start, "end", state, cores, tasks, origin, placement IS NOT NULL
                   FROM attempts WHERE job = ?1"#,
                params![id],
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
        let home = StoreHome::new();
        let mut store = home.open(2);
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
            .query_row("SELECT count(*) FROM users", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(slots, 1);
    }

    #[test]
    fn new_users_come_out_once_and_everyone_comes_out_after_a_reopen() {
        let home = StoreHome::new();
        let mut store = home.open(2);
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
            home.open(2).new_users().unwrap(),
            [user("otter", None, 0), user("heron", Some("Grey Heron"), 1)]
        );
    }

    #[test]
    fn node_health_is_written_on_change_only() {
        let home = StoreHome::new();
        let mut store = home.open(2);
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
        let mut store = home.open(2);
        store
            .record(
                UnixTime(3),
                &[Event::Nodes(vec![node(NodeState::Draining)])],
            )
            .unwrap();
        let silent = Node {
            flags: vec![NodeFlag::NotResponding],
            ..node(NodeState::Draining)
        };
        store
            .record(UnixTime(4), &[Event::Nodes(vec![silent])])
            .unwrap();
        let states: Vec<(i64, String)> = store
            .db
            .query_map(
                "SELECT at, state FROM node_states ORDER BY at",
                params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            states,
            [
                (0, "up".into()),
                (2, "draining".into()),
                (4, "draining*".into())
            ]
        );
        let history = store.history();
        let silent: Vec<_> = history
            .node_health(UnixTime(4))
            .unwrap()
            .into_iter()
            .map(|health| (health.state, health.not_responding))
            .collect();
        assert_eq!(
            silent,
            [
                (None, false),
                (Some(NodeState::Draining), false),
                (Some(NodeState::Draining), true)
            ]
        );
    }

    /// Makes every write to `store` fail, or work again.
    fn refuse_writes(store: &Store, refuse: bool) {
        let switch = if refuse { "ON" } else { "OFF" };
        let setting = match store.db {
            Connection::Sqlite(_) => "PRAGMA query_only",
            Connection::Timescale(_) => "SET default_transaction_read_only",
        };
        store
            .db
            .execute_batch(&format!("{setting} = {switch}"))
            .unwrap();
    }

    #[test]
    fn a_failed_write_opens_a_gap() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        assert!(!store.sampled(UnixTime(T0), 300).unwrap());
        refuse_writes(&store, true);
        let started = [Event::Started {
            attempt: running(1),
        }];
        assert!(store.record(UnixTime(T0 + 10), &started).is_err());
        refuse_writes(&store, false);
        store.record(UnixTime(T0 + 20), &[]).unwrap();
        assert!(
            store.sampled(UnixTime(T0 + 20), 300).unwrap(),
            "the start at +10 s never reached the store"
        );
        let gap = store
            .next_backfill(UnixTime(T0 + 20), 300, DAY_S)
            .unwrap()
            .unwrap();
        assert_eq!((gap.from.0, gap.to.0, gap.gap), (T0 - 300, T0 + 20, true));
    }

    #[test]
    fn a_node_health_change_rolled_back_with_its_tick_is_written_by_the_next() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        let listing = |state| Event::Nodes(vec![node(state)]);
        store
            .record(UnixTime(T0), &[listing(NodeState::Idle)])
            .unwrap();
        let unstorable = Attempt {
            mem_mb: Some(u64::MAX),
            ..running(1)
        };
        let tick = [
            listing(NodeState::Drained),
            Event::Started {
                attempt: unstorable,
            },
        ];
        assert!(store.record(UnixTime(T0 + 10), &tick).is_err());
        store
            .record(UnixTime(T0 + 20), &[listing(NodeState::Drained)])
            .unwrap();
        let states: Vec<(i64, String)> = store
            .db
            .query_map(
                "SELECT at, state FROM node_states ORDER BY at",
                params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(states, [(T0, "up".into()), (T0 + 20, "drained".into())]);
    }

    #[test]
    fn cores_are_counted_by_the_first_node_listing_that_is_written() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        let started = [Event::Started {
            attempt: running(1),
        }];
        store.record(UnixTime(T0), &started).unwrap();
        assert_eq!(row(&store, 1).3, None, "no node listing yet");
        let listing = [Event::Nodes(vec![node(NodeState::Mixed)])];
        refuse_writes(&store, true);
        assert!(store.record(UnixTime(T0 + 10), &listing).is_err());
        refuse_writes(&store, false);
        store.record(UnixTime(T0 + 20), &listing).unwrap();
        assert_eq!(row(&store, 1).3, Some(4));
    }

    #[test]
    fn a_new_store_backfills_a_stretch_at_a_time_to_the_horizon() {
        let home = StoreHome::new();
        let mut store = home.open(2);
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
        let home = StoreHome::new();
        let mut store = home.open(2);
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
        let mut store = home.open(2);
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
            .query_row(
                "SELECT count(*) FROM attempts WHERE job = 9",
                params![],
                |row| row.get(0),
            )
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
        let home = StoreHome::new();
        let mut store = home.open(2);
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

    #[test]
    fn a_gap_window_applied_after_min_job_age_still_closes_the_gap() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        store.sampled(UnixTime(T0), 300).unwrap();
        while let Some(window) = store.next_backfill(UnixTime(T0), 300, DAY_S).unwrap() {
            store
                .backfill(window, &[], &HashSet::new(), UnixTime(T0))
                .unwrap();
        }
        let back = UnixTime(T0 + 3 * 3_600);
        assert!(store.sampled(back, 300).unwrap());
        let gap = store.next_backfill(back, 300, DAY_S).unwrap().unwrap();
        let late = UnixTime(back.0 + 900);
        for tick in (back.0..=late.0).step_by(10) {
            assert!(!store.sampled(UnixTime(tick), 300).unwrap());
        }

        store.backfill(gap, &[], &HashSet::new(), late).unwrap();
        assert_eq!(store.next_backfill(late, 300, DAY_S).unwrap(), None);
        assert!(!store.sampled(UnixTime(late.0 + 10), 300).unwrap());
        assert_eq!(
            store.meta("complete_to").unwrap(),
            Some(UnixTime(late.0 + 10))
        );
    }

    #[test]
    fn a_window_toward_the_horizon_applied_after_a_gap_opens_moves_only_the_horizon() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        store.sampled(UnixTime(T0), 300).unwrap();
        store.sampled(UnixTime(T0 + 290), 300).unwrap();
        let past = store
            .next_backfill(UnixTime(T0 + 290), 300, DAY_S)
            .unwrap()
            .unwrap();
        let back = UnixTime(T0 + 3 * 3_600);
        assert!(store.sampled(back, 300).unwrap());

        store.backfill(past, &[], &HashSet::new(), back).unwrap();
        let gap = store.next_backfill(back, 300, DAY_S).unwrap().unwrap();
        assert_eq!((gap.from.0, gap.to), (T0 - 10, back));
        store.backfill(gap, &[], &HashSet::new(), back).unwrap();
        let next = store.next_backfill(back, 300, DAY_S).unwrap().unwrap();
        assert_eq!(
            (next.from.0, next.to.0),
            (T0 - 2 * DAY_S, T0 - DAY_S),
            "the horizon window moved the history back a day"
        );
    }

    fn core_s(store: &Store) -> i64 {
        store
            .db
            .query_row(
                "SELECT CAST(coalesce(sum(core_s), 0) AS BIGINT) FROM usage",
                params![],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn bins_roll_up_after_the_grace_and_hold_every_core_second() {
        let home = StoreHome::new();
        let mut store = home.open(2);
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
            .query_row(
                "SELECT CAST(sum(started) + sum(ended) AS BIGINT) FROM usage",
                params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(starts_and_ends, 2);
    }

    #[test]
    fn usage_sums_closed_bins_by_step_and_rolls_up_open_ones_on_the_spot() {
        let home = StoreHome::new();
        let mut store = home.open(2);
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

        let history = store.history();
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
                .map(|step| (step.at.0 - T0, step.key, step.core_s))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            usage(By::Login),
            [
                (0, "heron".into(), 2_400),
                (0, "otter".into(), 2_400),
                (600, "otter".into(), 1_600),
            ],
            "otter's last 100 s come from the open bin, and heron has nothing left in it"
        );
        assert_eq!(
            usage(By::Partition),
            [
                (0, "4hours".into(), 2_400),
                (0, "long".into(), 2_400),
                (600, "4hours".into(), 1_600),
            ]
        );
    }

    #[test]
    fn usage_counts_open_bins_up_to_the_end_asked_for() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Nodes(vec![node(NodeState::Mixed)]),
                    Event::Started {
                        attempt: running(1),
                    },
                ],
            )
            .unwrap();
        store.sampled(UnixTime(T0), 300).unwrap();
        let query = UsageQuery {
            from: UnixTime(T0),
            to: UnixTime(T0 + 450),
            step_s: 300,
            by: By::Login,
        };
        let core_s: Vec<(i64, u64)> = store
            .history()
            .usage(&query, UnixTime(T0 + 1_000))
            .unwrap()
            .into_iter()
            .map(|step| (step.at.0 - T0, step.core_s))
            .collect();
        assert_eq!(core_s, [(0, 4 * 300), (300, 4 * 150)]);
    }

    #[test]
    fn overlapping_attempts_come_through_the_index_named() {
        let home = StoreHome::sqlite();
        let store = home.open(2);
        let plan = |index| -> String {
            let sql = format!("EXPLAIN QUERY PLAN {}", overlapping("job", "< ?2", index));
            let details = store
                .db
                .query_map(&sql, params![0_i64, 0_i64], |row| row.get::<String>(3))
                .unwrap();
            details.join("; ")
        };
        let near = plan(IndexChoice::ByEnd);
        assert!(near.contains("attempts_by_end"), "{near}");
        assert!(!near.contains("attempts_by_submit"), "{near}");
        let far = plan(IndexChoice::BySubmit);
        let (first_half, _) = far.split_once("UNION ALL").unwrap();
        assert!(first_half.contains("attempts_by_submit"), "{far}");
        assert!(!first_half.contains("attempts_by_end"), "{far}");
    }

    #[test]
    fn overlapping_attempts_are_the_same_whichever_index_finds_them() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        let mut events = vec![Event::Nodes(vec![node(NodeState::Mixed)])];
        for id in 0..40 {
            let start = T0 + i64::from(id) * 300;
            let end = (id % 3 != 0).then_some(start + 1_000);
            let state = if end.is_some() {
                JobState::Ended(EndState::Completed)
            } else {
                JobState::Running
            };
            events.push(Event::Started {
                attempt: Attempt {
                    key: AttemptKey {
                        job: JobId(id),
                        submit: UnixTime(start - 60),
                    },
                    ..attempt(id, state, Some(start), end)
                },
            });
        }
        store.record(UnixTime(T0), &events).unwrap();
        for (from, to) in [
            (T0 + 3_000, T0 + 3_300),
            (T0, T0),
            (T0 + 20_000, T0 + 30_000),
        ] {
            let keys = |index| {
                let mut jobs: Vec<u32> = store
                    .db
                    .query_map(
                        &overlapping("job", "< ?2", index),
                        params![from, to],
                        |row| row.get(0),
                    )
                    .unwrap();
                jobs.sort_unstable();
                jobs
            };
            let by_end = keys(IndexChoice::ByEnd);
            assert_ne!(by_end, Vec::<u32>::new(), "{from}..{to}");
            assert_eq!(keys(IndexChoice::BySubmit), by_end, "{from}..{to}");
            assert_eq!(keys(IndexChoice::Planner), by_end, "{from}..{to}");
        }
    }

    #[test]
    fn the_index_follows_where_attempts_are_dense() {
        let home = StoreHome::sqlite();
        let mut store = home.open(2);
        let ended = |id: u32, submit: i64| Attempt {
            key: AttemptKey {
                job: JobId(id),
                submit: UnixTime(submit),
            },
            ..attempt(
                id,
                JobState::Ended(EndState::Completed),
                Some(submit + 60),
                Some(submit + 600),
            )
        };
        let old = (0..5).map(|id| ended(id, T0 - 90 * DAY_S));
        let recent = (5..500).map(|id| ended(id, T0 - DAY_S + i64::from(id)));
        let events: Vec<Event> = old
            .chain(recent)
            .map(|attempt| Event::Started { attempt })
            .collect();
        store.record(UnixTime(T0), &events).unwrap();
        store
            .set_meta("history_from", UnixTime(T0 - 100 * DAY_S))
            .unwrap();
        store.set_meta("complete_to", UnixTime(T0)).unwrap();
        let at = UnixTime(T0 - 45 * DAY_S);
        assert_eq!(
            index_choice(&store.db, at, at).unwrap(),
            IndexChoice::BySubmit,
            "5 old, 495 recent"
        );
        let near = UnixTime(T0 - 600);
        assert_eq!(
            index_choice(&store.db, near, near).unwrap(),
            IndexChoice::ByEnd
        );
    }

    #[test]
    fn prune_keeps_open_attempts_and_recent_history() {
        let home = StoreHome::new();
        let mut store = home.open(2);
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
            .query_map("SELECT job FROM attempts", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(left, [2]);
    }

    #[test]
    fn opening_an_old_store_clears_starts_without_nodes_and_rolls_their_bins_again() {
        let home = StoreHome::sqlite();
        let mut store = home.open(2);
        store.sampled(UnixTime(T0), 300).unwrap();
        let cancelled = Attempt {
            key: AttemptKey {
                job: JobId(5),
                submit: UnixTime(T0 + 10),
            },
            nodes: Vec::new(),
            ..attempt(
                5,
                JobState::Ended(EndState::Cancelled),
                Some(4_294_967_294),
                Some(T0 + 310),
            )
        };
        store
            .record(
                UnixTime(T0),
                &[
                    Event::Nodes(vec![node(NodeState::Idle)]),
                    Event::Started { attempt: cancelled },
                ],
            )
            .unwrap();
        store.roll(UnixTime(T0 + 900 + GRACE_S)).unwrap();
        let wait_s = |store: &Store| -> i64 {
            store
                .db
                .query_row(
                    "SELECT CAST(sum(wait_s) AS BIGINT) FROM usage",
                    params![],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(wait_s(&store), 890, "the start in 2106 kept it waiting");
        store.db.execute_batch("PRAGMA user_version = 2").unwrap();
        drop(store);

        let store = home.open(2);
        assert_eq!(row(&store, 5).0, None);
        assert_eq!(wait_s(&store), 300);
        assert_eq!(store.meta("stale_from").unwrap(), None);
    }

    #[test]
    fn a_store_from_a_newer_glurmo_does_not_open() {
        let home = StoreHome::sqlite();
        rusqlite::Connection::open(home.path())
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        let error = Store::open(&home.path(), home.settings(2)).err().unwrap();
        assert!(format!("{error:#}").contains("99"), "{error:#}");
    }

    #[test]
    fn prune_keeps_the_health_each_node_had_at_the_cutoff() {
        let home = StoreHome::new();
        let mut store = home.open(2);
        let named = |name: &str, state| Node {
            name: NodeName(name.into()),
            ..node(state)
        };
        let listings = [
            (T0 - 3 * DAY_S, NodeState::Drained),
            (T0 - 2 * DAY_S, NodeState::Idle),
        ];
        for (at, state) in listings {
            let nodes = vec![named("z1", NodeState::Drained), named("z2", state)];
            store.record(UnixTime(at), &[Event::Nodes(nodes)]).unwrap();
        }
        store.prune(UnixTime(T0 - DAY_S)).unwrap();
        let left: Vec<(String, i64)> = store
            .db
            .query_map(
                "SELECT node, at FROM node_states ORDER BY node, at",
                params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            left,
            [("z1".into(), T0 - 3 * DAY_S), ("z2".into(), T0 - 2 * DAY_S)]
        );
    }

    #[test]
    fn backup_is_a_readable_copy() {
        let home = StoreHome::sqlite();
        let mut store = home.open(2);
        store
            .record(
                UnixTime(T0),
                &[Event::Started {
                    attempt: running(1),
                }],
            )
            .unwrap();
        let dest = home.dir().join("backup/copy.sqlite");
        backup(&home.path(), &dest).unwrap();
        backup(&home.path(), &dest).unwrap();
        let copied: u32 = rusqlite::Connection::open(&dest)
            .unwrap()
            .query_row("SELECT count(*) FROM attempts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(copied, 1);
    }

    #[tokio::test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    async fn timescale_reads_back_what_sqlite_does_after_the_busy_quarter_hour() {
        let (_sqlite_home, hub, sqlite) = busy_store_in(StoreHome::sqlite()).await;
        let (_timescale_home, _, timescale) = busy_store_in(StoreHome::timescale()).await;
        assert_timescale_reads_what_sqlite_does(&hub, &sqlite, &timescale);
    }

    #[tokio::test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    async fn a_copy_into_timescale_reads_back_what_the_sqlite_store_does_and_carries_on() {
        let (sqlite_home, hub, sqlite) = busy_store_in(StoreHome::sqlite()).await;
        let timescale_home = StoreHome::timescale();
        let server = timescale_home.server().unwrap();
        let copied = copy(&sqlite_home.path(), &server).unwrap();
        let tables: Vec<&str> = copied.iter().map(|rows| rows.table).collect();
        assert_eq!(
            tables,
            ["users", "node_states", "meta", "attempts", "usage"]
        );
        for rows in copied {
            assert!(rows.source > 0, "{rows:?}");
            assert_eq!(rows.source, rows.target, "{rows:?}");
        }

        let mut store = timescale_home.open(0);
        assert_timescale_reads_what_sqlite_does(&hub, &sqlite, &store.history());
        assert_eq!(
            store.new_users().unwrap(),
            sqlite_home.open(0).new_users().unwrap(),
            "everyone keeps their color"
        );
        let last = hub.deltas().last().unwrap().at;
        assert!(
            store.sampled(UnixTime(last.0 + 10), 300).unwrap(),
            "the copy resumes with a gap for accounting to fill"
        );
        let refused = copy(&sqlite_home.path(), &server).unwrap_err();
        assert!(
            format!("{refused:#}").contains("already holds"),
            "{refused:#}"
        );
    }

    /// Checks that both histories of the busy quarter hour that `hub` published give the same answers.
    fn assert_timescale_reads_what_sqlite_does(hub: &Hub, sqlite: &History, timescale: &History) {
        let ticks: Vec<UnixTime> = hub.deltas().iter().map(|delta| delta.at).collect();
        let (first, last) = (ticks[0], *ticks.last().unwrap());
        let by_key = |mut attempts: Vec<Stored>| {
            attempts.sort_by_key(|stored| (stored.attempt.key.job.0, stored.attempt.key.submit.0));
            attempts
        };
        let by_time = |mut changes: Vec<NodeHealth>| {
            changes.sort_by_key(|health| (health.at, health.node.0.clone()));
            changes
        };

        assert_eq!(timescale.starts().unwrap(), sqlite.starts().unwrap());
        for by in [By::Login, By::Partition] {
            for step_s in [300, 900] {
                let query = UsageQuery {
                    from: UnixTime(first.0 - 600),
                    to: UnixTime(last.0 + 600),
                    step_s,
                    by,
                };
                let usage = sqlite.usage(&query, last).unwrap();
                assert_ne!(usage, []);
                assert_eq!(timescale.usage(&query, last).unwrap(), usage);
            }
        }
        for at in [first, UnixTime(first.0.midpoint(last.0)), last] {
            let alive = by_key(sqlite.alive(at).unwrap());
            assert!(alive.len() > 100, "{} alive", alive.len());
            assert_eq!(by_key(timescale.alive(at).unwrap()), alive);
        }
        let touched = by_key(sqlite.touched(first, last).unwrap());
        assert!(touched.len() > 100, "{} touched", touched.len());
        assert_eq!(by_key(timescale.touched(first, last).unwrap()), touched);
        let health = by_time(sqlite.node_health(last).unwrap());
        assert_ne!(health, []);
        assert_eq!(by_time(timescale.node_health(last).unwrap()), health);
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn timescale_keeps_30_day_chunks_and_its_policies_compress_the_old_ones() {
        let home = StoreHome::timescale();
        let mut store = home.open(2);
        let chunks: Vec<(String, i64, String)> = store
            .db
            .query_map(
                "SELECT hypertable_name::text, integer_interval, integer_now_func::text
                 FROM timescaledb_information.dimensions
                 WHERE hypertable_schema = current_schema() ORDER BY hypertable_name",
                params![],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            chunks,
            [
                ("attempts".into(), 30 * DAY_S, "unix_now".into()),
                ("usage".into(), 30 * DAY_S, "unix_now".into())
            ]
        );

        let ranged: Vec<String> = store
            .db
            .query_map(
                "SELECT entry->>'column'
                 FROM timescaledb_information.hypertable_compression_settings,
                      jsonb_array_elements(index) AS entry
                 WHERE hypertable = 'attempts'::regclass
                       AND entry->>'type' = 'minmax' AND entry->>'source' = 'config'
                 ORDER BY 1",
                params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            ranged,
            ["end", "start"],
            "a read of a stretch of time skips the compressed batches outside it"
        );

        let long_ago = T0 - 60 * DAY_S;
        let old = Attempt {
            key: AttemptKey {
                job: JobId(1),
                submit: UnixTime(long_ago),
            },
            ..attempt(
                1,
                JobState::Ended(EndState::Completed),
                Some(long_ago + 60),
                Some(long_ago + 600),
            )
        };
        let window = Window {
            from: UnixTime(long_ago),
            to: UnixTime(long_ago + 900),
            gap: false,
        };
        store
            .record(UnixTime(T0), &[Event::Nodes(vec![node(NodeState::Mixed)])])
            .unwrap();
        store.sampled(UnixTime(T0), 300).unwrap();
        store
            .backfill(window, &[old], &HashSet::new(), UnixTime(T0))
            .unwrap();
        assert_eq!(core_s(&store), 4 * 540);

        let policies: Vec<i64> = store
            .db
            .query_map(
                "SELECT job_id::bigint FROM timescaledb_information.jobs
                 WHERE hypertable_schema = current_schema() AND proc_name = 'policy_compression'",
                params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(policies.len(), 2);
        // The server also runs a new policy by itself, and whichever run loses the race to a chunk fails.
        for policy in policies {
            let _ = store.db.execute_batch(&format!("CALL run_job({policy})"));
        }
        let old_chunks: Vec<(String, bool)> = store
            .db
            .query_map(
                "SELECT hypertable_name::text, is_compressed FROM timescaledb_information.chunks
                 WHERE hypertable_schema = current_schema() AND range_end_integer < ?1
                 ORDER BY hypertable_name",
                params![T0 - 8 * DAY_S],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            old_chunks,
            [("attempts".into(), true), ("usage".into(), true)]
        );
        assert_eq!(core_s(&store), 4 * 540);
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn timescale_writes_reach_compressed_chunks() {
        let home = StoreHome::timescale();
        let mut store = home.open(2);
        let ended = attempt(
            3,
            JobState::Ended(EndState::Completed),
            Some(T0 - 300),
            Some(T0 - 100),
        );
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
                    Event::Started { attempt: ended },
                ],
            )
            .unwrap();
        store.sampled(UnixTime(T0), 300).unwrap();
        store.roll(UnixTime(T0 + 300 + GRACE_S)).unwrap();
        let compressed: usize = store
            .db
            .query_row(
                "SELECT count(compress_chunk(chunk)) FROM (
                     SELECT show_chunks('attempts') UNION ALL SELECT show_chunks('usage')
                 ) AS chunks (chunk)",
                params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(compressed, 2);
        assert_eq!(core_s(&store), 4 * (300 + 300));

        let end = [Event::Ended {
            key: key(1),
            at: UnixTime(T0 + 400),
            state: Some(EndState::Completed),
        }];
        store.record(UnixTime(T0 + 400), &end).unwrap();
        assert_eq!(row(&store, 1).1, Some(T0 + 400), "an update");
        let timeout = attempt(
            2,
            JobState::Ended(EndState::Timeout),
            Some(T0 - 300),
            Some(T0 + 100),
        );
        let window = Window {
            from: UnixTime(T0),
            to: UnixTime(T0 + 600),
            gap: false,
        };
        store
            .backfill(window, &[timeout], &HashSet::new(), UnixTime(T0 + 600))
            .unwrap();
        assert_eq!(row(&store, 2).2.as_deref(), Some("timeout"), "an upsert");
        assert_eq!(core_s(&store), 4 * (300 + 100), "a rollup");
        store.prune(UnixTime(T0)).unwrap();
        let left: Vec<u32> = store
            .db
            .query_map("SELECT job FROM attempts ORDER BY job", params![], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(left, [1, 2], "a delete");
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn timescale_picks_its_own_index_for_overlapping_attempts() {
        let home = StoreHome::timescale();
        let store = home.open(2);
        let index = index_choice(&store.db, UnixTime(T0), UnixTime(T0)).unwrap();
        assert_eq!(index, IndexChoice::Planner);
        assert!(!overlapping("job", "< ?2", index).contains('+'));
    }

    #[test]
    #[ignore = "needs the TimescaleDB server that GLURMO_TEST_TIMESCALE names"]
    fn a_timescale_store_from_a_newer_glurmo_does_not_open() {
        let home = StoreHome::timescale();
        home.open(2).set_meta("schema", UnixTime(99)).unwrap();
        let error = Store::connect(&home.server().unwrap(), home.settings(2))
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("99"), "{error:#}");
    }

    #[test]
    fn the_longest_mount_decides_the_filesystem() {
        let mounts = "\
/dev/sda1 / ext4 rw 0 0
10.0.0.1:/ /home ceph rw 0 0
/dev/sdb1 /scratch ext4 rw 0 0
";
        assert_eq!(
            mount_kind(Path::new("/home/otter/glurmo"), mounts),
            Some("ceph")
        );
        assert_eq!(
            mount_kind(Path::new("/scratch/otter"), mounts),
            Some("ext4")
        );
        assert_eq!(mount_kind(Path::new("/homework"), mounts), Some("ext4"));
    }
}
