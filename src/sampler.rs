//! The tick loop. It runs the commands that are due, diffs their output, looks up placements and
//! publishes each tick's changes to the [`Hub`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;

use crate::config::Names;
use crate::diff::{Known, placed};
use crate::hub::Hub;
use crate::model::{Attempt, Event, Login, Outage, UnixTime, User};
use crate::passwd;
use crate::slurm::source::Exhausted;
use crate::slurm::{Intervals, Lookup, Slurm};
use crate::store::Store;

/// A call slower than this counts against its command, like a failure.
const SLOW: Duration = Duration::from_secs(2);

/// How far a command's interval can stretch under backoff.
const MAX_SLOWDOWN: u64 = 8;

/// Healthy calls in a row that halve a stretched interval again.
const HEALTHY_TO_RECOVER: u32 = 3;

/// The shortest and longest stretch of accounting read at once. sacct takes about 0.5 ms per row,
/// and a busy day on zlurm holds 27,000 rows, which runs past the command timeout.
const BACKFILL_SPAN_S: std::ops::RangeInclusive<i64> = 900..=86_400;

/// A history read faster than this doubles the next window. One slower than [`SLOW_BACKFILL`] halves it.
const FAST_BACKFILL: Duration = Duration::from_secs(2);
const SLOW_BACKFILL: Duration = Duration::from_secs(5);

/// The next backfill window's length, after a read of `span_s` that took `took` and worked or not.
fn next_span(span_s: i64, worked: bool, took: Duration) -> i64 {
    let span_s = if !worked || took > SLOW_BACKFILL {
        span_s / 2
    } else if took < FAST_BACKFILL {
        span_s * 2
    } else {
        span_s
    };
    span_s.clamp(*BACKFILL_SPAN_S.start(), *BACKFILL_SPAN_S.end())
}

/// How one command has fared since the sampler started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Calls {
    /// Calls made.
    pub count: u64,
    /// Calls that failed.
    pub failures: u64,
    /// How long the last call took.
    pub last_ms: Option<u64>,
    /// Why the last failed call failed.
    pub last_error: Option<String>,
    /// Whether the last call failed.
    pub failing: bool,
}

impl Calls {
    /// Runs one call and counts it. Also returns whether it was healthy: it worked, and within 2 s.
    async fn time<T>(&mut self, call: impl Future<Output = Result<T>>) -> (Result<T>, bool) {
        let started = Instant::now();
        let result = call.await;
        let took = started.elapsed();
        self.count += 1;
        self.last_ms = Some(u64::try_from(took.as_millis()).unwrap_or(u64::MAX));
        self.failing = result.is_err();
        if let Err(error) = &result {
            self.failures += 1;
            self.last_error = Some(format!("{error:#}"));
        }
        let healthy = result.is_ok() && took <= SLOW;
        (result, healthy)
    }
}

/// One command's calls, and for a command that runs on a schedule, its interval under backoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CommandHealth {
    /// The command, such as `squeue`.
    pub command: &'static str,
    /// Seconds between calls now, or `None` for a command that runs only when needed.
    pub every_s: Option<u64>,
    /// How its calls went.
    #[serde(flatten)]
    pub calls: Calls,
}

/// How every command has fared since the sampler started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Health(pub Vec<CommandHealth>);

fn unpaced(command: &'static str, calls: &Calls) -> CommandHealth {
    CommandHealth {
        command,
        every_s: None,
        calls: calls.clone(),
    }
}

/// When one command runs next, how far backoff has stretched its interval, and how its calls went.
struct Pace {
    every_s: u64,
    most_s: u64,
    slowdown: u64,
    healthy: u32,
    next: UnixTime,
    calls: Calls,
}

impl Pace {
    fn new(every_s: u64, most_s: u64) -> Self {
        Self {
            every_s,
            most_s,
            slowdown: 1,
            healthy: 0,
            next: UnixTime(0),
            calls: Calls::default(),
        }
    }

    fn is_due(&self, now: UnixTime) -> bool {
        now >= self.next
    }

    /// The interval with backoff applied.
    fn interval_s(&self) -> u64 {
        (self.every_s * self.slowdown).min(self.most_s)
    }

    /// Schedules the next call. A failed or slow call doubles the interval, up to 8×,
    /// and three healthy calls in a row halve it.
    fn record(&mut self, now: UnixTime, healthy: bool) {
        if healthy {
            self.healthy += 1;
            if self.healthy == HEALTHY_TO_RECOVER {
                self.slowdown = (self.slowdown / 2).max(1);
                self.healthy = 0;
            }
        } else {
            self.slowdown = (self.slowdown * 2).min(MAX_SLOWDOWN);
            self.healthy = 0;
        }
        let interval = i64::try_from(self.interval_s()).unwrap_or(i64::MAX);
        self.next = UnixTime(now.0.saturating_add(interval));
    }

    /// Runs one call and paces the next by how it went.
    async fn time<T>(&mut self, now: UnixTime, call: impl Future<Output = Result<T>>) -> Result<T> {
        let (result, healthy) = self.calls.time(call).await;
        self.record(now, healthy);
        result
    }

    fn health(&self, command: &'static str) -> CommandHealth {
        CommandHealth {
            command,
            every_s: Some(self.interval_s()),
            calls: self.calls.clone(),
        }
    }
}

/// Samples the cluster on a schedule and publishes what changed.
pub struct Sampler {
    slurm: Slurm,
    known: Known,
    jobs: Pace,
    nodes: Pace,
    partitions: Pace,
    stats: Pace,
    placing: Calls,
    looking_up: Calls,
    settling: Calls,
    backfilling: Calls,
    /// Started attempts that still need a placement lookup, oldest first.
    lookups: VecDeque<Attempt>,
    budget: u64,
    /// Whether no jobs tick has succeeded yet, so one listing of every placement replaces the lookups.
    cold: bool,
    /// Whether an outage is on.
    in_outage: bool,
    hub: Hub,
    store: Option<Store>,
    names: Names,
    /// How much accounting the next backfill reads.
    backfill_span_s: i64,
}

impl Sampler {
    /// A sampler that runs each command at its interval, publishes to `hub`, keeps history in `store` if
    /// there is one, and names users as `names` says.
    /// The jobs interval stays at most `MinJobAge / 2` under backoff, so no ended job leaves squeue unseen.
    pub fn new(
        slurm: Slurm,
        intervals: Intervals,
        store: Option<Store>,
        hub: Hub,
        names: Names,
    ) -> Self {
        let jobs_most_s = (slurm.site().min_job_age_s / 2).max(intervals.jobs_s);
        Self {
            slurm,
            known: Known::default(),
            jobs: Pace::new(intervals.jobs_s, jobs_most_s),
            nodes: Pace::new(intervals.nodes_s, u64::MAX),
            partitions: Pace::new(intervals.partitions_s, u64::MAX),
            stats: Pace::new(intervals.stats_s, u64::MAX),
            placing: Calls::default(),
            looking_up: Calls::default(),
            settling: Calls::default(),
            backfilling: Calls::default(),
            lookups: VecDeque::new(),
            budget: intervals.lookups,
            cold: true,
            in_outage: false,
            hub,
            store,
            names,
            backfill_span_s: 21_600,
        }
    }

    /// Ticks until `until`, or forever when it is `None`. A replay also stops once it runs out of answers.
    /// A failed tick is logged and skipped. An [`Outage`] lasts while the last call of any listing failed.
    pub async fn run(&mut self, until: Option<UnixTime>) {
        loop {
            let at = self.slurm.now();
            let mut events = match self.tick(at).await {
                Ok(events) => events,
                Err(error) if error.is::<Exhausted>() => return,
                Err(error) => {
                    eprintln!("tick at {} skipped: {error:#}", at.0);
                    Vec::new()
                }
            };
            let failing = self.failing();
            if failing.is_some() != self.in_outage {
                self.in_outage = failing.is_some();
                events.push(Event::Outage(
                    failing.map(|error| Outage { since: at, error }),
                ));
            }
            self.hub.publish(at, events);
            self.hub.report(self.health());
            // A replay never waits on its own, and subscribers in the same task need a turn.
            tokio::task::yield_now().await;
            let next = self.next_due();
            if until.is_some_and(|until| next > until) {
                return;
            }
            self.slurm.sleep_until(next).await;
        }
    }

    /// Why the first listing whose last call failed failed, if any did.
    fn failing(&self) -> Option<String> {
        [
            &self.jobs.calls,
            &self.nodes.calls,
            &self.partitions.calls,
            &self.stats.calls,
            &self.placing,
        ]
        .into_iter()
        .find(|calls| calls.failing)
        .and_then(|calls| calls.last_error.clone())
    }

    fn health(&self) -> Health {
        Health(vec![
            self.jobs.health("squeue"),
            self.nodes.health("sinfo"),
            self.partitions.health("scontrol show partition"),
            self.stats.health("sdiag"),
            unpaced("scontrol show job", &self.placing),
            unpaced("scontrol show job ID", &self.looking_up),
            unpaced("sacct -j", &self.settling),
            unpaced("sacct -S -E", &self.backfilling),
        ])
    }

    fn next_due(&self) -> UnixTime {
        [&self.jobs, &self.nodes, &self.partitions, &self.stats]
            .iter()
            .map(|pace| pace.next)
            .min()
            .unwrap_or_default()
    }

    /// Runs the due commands, then diffs their output. If any fails, the tick is dropped,
    /// so a job counts as gone only after a squeue that worked.
    async fn tick(&mut self, now: UnixTime) -> Result<Vec<Event>> {
        let nodes = if self.nodes.is_due(now) {
            Some(self.nodes.time(now, self.slurm.nodes()).await?)
        } else {
            None
        };
        let partitions = if self.partitions.is_due(now) {
            Some(self.partitions.time(now, self.slurm.partitions()).await?)
        } else {
            None
        };
        let jobs = if self.jobs.is_due(now) {
            Some(self.jobs.time(now, self.slurm.jobs()).await?)
        } else {
            None
        };
        let placements = if jobs.is_some() && self.cold {
            let nodes = nodes.as_deref().unwrap_or(self.known.nodes());
            Some(self.placing.time(self.slurm.placements(nodes)).await.0?)
        } else {
            None
        };
        let stats = if self.stats.is_due(now) {
            Some(self.stats.time(now, self.slurm.stats()).await?)
        } else {
            None
        };

        let listed = jobs.is_some();
        let mut events = Vec::new();
        events.extend(nodes.and_then(|nodes| self.known.diff_nodes(nodes)));
        events.extend(partitions.and_then(|partitions| self.known.diff_partitions(partitions)));
        if let Some(jobs) = jobs {
            let changes = self.known.diff_jobs(&jobs, now);
            self.lookups
                .extend(changes.iter().filter_map(|event| match event {
                    Event::Started { attempt } => Some(attempt.clone()),
                    _ => None,
                }));
            events.extend(changes);
            match placements {
                Some(placements) => {
                    for found in placements {
                        let queued = self.lookups.iter().position(|a| a.key.job == found.job);
                        if let Some(attempt) = queued.and_then(|i| self.lookups.remove(i)) {
                            events.extend(placed(&attempt, found));
                        }
                    }
                    self.cold = false;
                }
                None => events.extend(self.look_up().await),
            }
            events.extend(self.settle().await);
        }
        events.extend(stats.map(Event::Stats));
        match self.archive(now, &events, listed).await {
            Ok(users) if !users.is_empty() => events.push(Event::Users(self.name(users).await)),
            Ok(_) => {}
            Err(error) => eprintln!("store at {}: {error:#}", now.0),
        }
        Ok(events)
    }

    /// Writes a tick's events to the store. After a jobs tick it also closes attempts that vanished
    /// while no listing came in, because the sampler or the controller was down. Then it rolls up
    /// closed bins, reads one window of accounting and runs the daily upkeep. A failure here never
    /// drops the tick. Returns the users seen for the first time.
    async fn archive(
        &mut self,
        now: UnixTime,
        events: &[Event],
        listed: bool,
    ) -> Result<Vec<User>> {
        let Some(store) = &mut self.store else {
            return Ok(Vec::new());
        };
        store.record(now, events)?;
        if !listed {
            return Ok(Vec::new());
        }
        let active = self.known.active();
        let min_job_age_s = i64::try_from(self.slurm.site().min_job_age_s)?;
        if store.sampled(now, min_job_age_s)? {
            let closed = store.recover(&active)?;
            if closed > 0 {
                eprintln!("{closed} attempts left the queue while no listing came in");
            }
        }
        store.roll(now)?;
        if let Some(window) = store.next_backfill(now, min_job_age_s, self.backfill_span_s)? {
            let started = Instant::now();
            let (attempts, _) = self
                .backfilling
                .time(self.slurm.history(window.from, window.to))
                .await;
            self.backfill_span_s =
                next_span(self.backfill_span_s, attempts.is_ok(), started.elapsed());
            store.backfill(window, &attempts?, &active, now)?;
        }
        store.maintain(now)?;
        store.new_users()
    }

    /// Gives new users their full names when the config asks for them, from the store or the system.
    async fn name(&mut self, mut users: Vec<User>) -> Vec<User> {
        if self.names == Names::Login {
            for user in &mut users {
                user.name = None;
            }
            return users;
        }
        let unnamed: Vec<Login> = users
            .iter()
            .filter(|user| user.name.is_none())
            .map(|user| user.login.clone())
            .collect();
        if unnamed.is_empty() {
            return users;
        }
        let names = match passwd::full_names(&unnamed).await {
            Ok(names) => names,
            Err(error) => {
                eprintln!("cannot look up full names: {error:#}");
                return users;
            }
        };
        if let Some(store) = &mut self.store
            && let Err(error) = store.name_users(&names)
        {
            eprintln!("cannot store full names: {error:#}");
        }
        for user in &mut users {
            if let Some(name) = names.get(&user.login) {
                user.name = Some(name.clone());
            }
        }
        users
    }

    /// Looks up queued placements, oldest first, up to the budget. A failed lookup waits for the next tick.
    async fn look_up(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for _ in 0..self.budget {
            let Some(attempt) = self.lookups.pop_front() else {
                break;
            };
            let lookup = self.slurm.lookup(&attempt, self.known.nodes());
            match self.looking_up.time(lookup).await.0 {
                Ok(Lookup::Found(found)) => events.extend(placed(&attempt, found)),
                Ok(Lookup::Gone) => {}
                Err(error) => {
                    eprintln!("lookup of {} failed: {error:#}", attempt.key.job.0);
                    self.lookups.push_front(attempt);
                    break;
                }
            }
        }
        events
    }

    /// Asks accounting how jobs that left squeue unseen ended. A failure leaves them for the next tick.
    async fn settle(&mut self) -> Vec<Event> {
        let jobs = self.known.unsettled();
        if jobs.is_empty() {
            return Vec::new();
        }
        match self.settling.time(self.slurm.outcomes(&jobs)).await.0 {
            Ok(outcomes) => self.known.settle(&outcomes),
            Err(error) => {
                eprintln!("sacct for {} jobs failed: {error:#}", jobs.len());
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use super::*;
    use crate::fixture::{busy_hub, busy_quarter_hour, busy_quarter_hour_with};
    use crate::model::{AttemptKey, Cluster, Delta, Owner};
    use crate::store::Settings;

    /// One line per delta: its time, how many events of each kind it holds, and how the cluster looks after it.
    fn summary(delta: &Delta, start: UnixTime, cluster: &Cluster) -> String {
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        for event in &delta.events {
            let kind = match event {
                Event::Started { .. } => "started".to_owned(),
                Event::Ended { state, .. } => format!("ended:{state:?}"),
                Event::Withdrawn { .. } => "withdrawn".to_owned(),
                Event::Settled { state, .. } => format!("settled:{state:?}"),
                Event::Placed { .. } => "placed".to_owned(),
                Event::Queued(attempts) => format!("queued:{}", attempts.len()),
                Event::Dequeued(keys) => format!("dequeued:{}", keys.len()),
                Event::Nodes(_) => "nodes".to_owned(),
                Event::Partitions(_) => "partitions".to_owned(),
                Event::Stats(_) => "stats".to_owned(),
                Event::Users(_) => "users".to_owned(),
                Event::Outage(_) => "outage".to_owned(),
            };
            *kinds.entry(kind).or_default() += 1;
        }
        let held = cluster
            .occupancy()
            .values()
            .flat_map(|cells| &cells.cores)
            .filter(|owner| matches!(owner, Some(Owner::Attempt(_))))
            .count();
        let kinds: Vec<String> = kinds
            .into_iter()
            .map(|(kind, count)| format!("{count} {kind}"))
            .collect();
        format!(
            "{} +{}s [{}] running {} holding {held} cores",
            delta.seq,
            delta.at.0 - start.0,
            kinds.join(", "),
            cluster.running.len()
        )
    }

    #[tokio::test]
    async fn store_agrees_with_the_deltas() {
        assert_store_agrees_with_the_deltas(busy_quarter_hour().await).await;
    }

    #[tokio::test]
    async fn store_agrees_with_the_deltas_after_a_controller_outage() {
        let mut listings = 0;
        let slurm = busy_quarter_hour_with(|exchange| {
            if exchange.argv[0] != "squeue" {
                return;
            }
            listings += 1;
            // Backoff spaces these seven failures over more than MinJobAge.
            if (25..32).contains(&listings) {
                exchange.status = 1;
                exchange.stdout.clear();
                exchange.stderr =
                    "squeue: error: Unable to contact slurm controller (connect failure)\n".into();
            }
        })
        .await;
        let (dir, deltas) = assert_store_agrees_with_the_deltas(slurm).await;
        let outages: Vec<Option<&str>> = deltas
            .iter()
            .flat_map(|delta| &delta.events)
            .filter_map(|event| match event {
                Event::Outage(outage) => Some(outage.as_ref().map(|outage| outage.error.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(outages.len(), 2, "{outages:?}");
        assert!(
            outages[0].is_some_and(|error| error.contains("Unable to contact slurm controller"))
        );
        assert_eq!(outages[1], None);
        let last = deltas.last().unwrap().at;
        let covered: i64 = rusqlite::Connection::open(dir.path().join("rtviz.sqlite"))
            .unwrap()
            .query_row(
                "SELECT value FROM meta WHERE key = 'complete_to'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            last.0 - covered > 300,
            "the store waits at the outage for accounting, which this replay lacks"
        );
    }

    /// Samples `slurm` into a new store, then checks the store against the deltas: open attempts that
    /// started are the running ones, every end landed, and open pending attempts hold the queue's tasks.
    /// Returns the store's directory and the deltas.
    async fn assert_store_agrees_with_the_deltas(
        slurm: Slurm,
    ) -> (tempfile::TempDir, Vec<std::sync::Arc<Delta>>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtviz.sqlite");
        let settings = Settings {
            backfill_days: 0,
            keep_days: None,
            backup: dir.path().join("backup.sqlite"),
        };
        let store = Store::open(&path, settings).unwrap();
        let hub = Hub::new(slurm.site().clone());
        let mut sampler = Sampler::new(
            slurm,
            Intervals::default(),
            Some(store),
            hub.clone(),
            Names::Login,
        );
        sampler.run(None).await;

        let mut cluster = Cluster::default();
        let mut ended = HashSet::new();
        let deltas = hub.deltas();
        for delta in &deltas {
            for event in &delta.events {
                if let Event::Ended { key, .. } = event {
                    ended.insert((key.job.0, key.submit.0));
                }
            }
            cluster.apply(delta);
        }
        let running: HashSet<(u32, i64)> = cluster
            .running
            .keys()
            .map(|key| (key.job.0, key.submit.0))
            .collect();
        let pending: u32 = cluster
            .queue
            .values()
            .map(|attempt| attempt.array.as_ref().map_or(1, |array| array.count))
            .sum();

        let db = rusqlite::Connection::open(&path).unwrap();
        let keys = |sql: &str| -> HashSet<(u32, i64)> {
            db.prepare(sql)
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            keys("SELECT job, submit FROM attempts WHERE start IS NOT NULL AND end IS NULL"),
            running
        );
        assert!(ended.is_subset(&keys(
            "SELECT job, submit FROM attempts WHERE end IS NOT NULL"
        )));
        let waiting: u32 = db
            .query_row(
                "SELECT sum(tasks) FROM attempts WHERE start IS NULL AND end IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(waiting, pending);
        let bins: u32 = db
            .query_row("SELECT count(DISTINCT bin) FROM usage", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(bins >= 2, "{bins} bins rolled up");
        let users: u32 = db
            .query_row("SELECT count(*) FROM users", [], |row| row.get(0))
            .unwrap();
        assert_eq!(usize::try_from(users).unwrap(), cluster.users.len());
        drop(db);
        (dir, deltas)
    }

    #[tokio::test]
    async fn replay_of_a_busy_quarter_hour() {
        let hub = busy_hub().await;

        let mut cluster = Cluster::default();
        let mut lines = Vec::new();
        let mut unplaced: HashSet<AttemptKey> = HashSet::new();
        let mut first_at = None;
        for delta in hub.deltas() {
            assert_eq!(delta.seq, u64::try_from(lines.len()).unwrap() + 1);
            for event in &delta.events {
                match event {
                    Event::Started { attempt } => {
                        unplaced.insert(attempt.key);
                    }
                    Event::Placed { key, .. } | Event::Ended { key, .. } => {
                        unplaced.remove(key);
                    }
                    _ => {}
                }
            }
            cluster.apply(&delta);
            let start = *first_at.get_or_insert(delta.at);
            lines.push(summary(&delta, start, &cluster));
        }
        assert!(lines.len() > 50, "only {} deltas", lines.len());
        assert_eq!(
            unplaced,
            HashSet::new(),
            "started and never placed or ended"
        );
        insta::assert_snapshot!(lines.join("\n"));
    }

    #[test]
    fn backoff_doubles_to_eight_times_and_recovers_after_three_healthy_calls() {
        let mut pace = Pace::new(10, 150);
        let intervals: Vec<i64> = [
            false, false, false, false, true, true, true, true, true, true,
        ]
        .into_iter()
        .map(|healthy| {
            pace.record(UnixTime(0), healthy);
            pace.next.0
        })
        .collect();
        assert_eq!(intervals, [20, 40, 80, 80, 80, 80, 40, 40, 40, 20]);
    }

    #[test]
    fn backfill_window_halves_when_slow_or_failed_and_doubles_when_fast() {
        let day = 86_400;
        let secs = Duration::from_secs;
        assert_eq!(next_span(21_600, true, secs(1)), 43_200);
        assert_eq!(next_span(day, true, secs(1)), day);
        assert_eq!(next_span(21_600, true, secs(3)), 21_600);
        assert_eq!(next_span(21_600, true, secs(9)), 10_800);
        assert_eq!(next_span(21_600, false, secs(15)), 10_800);
        assert_eq!(next_span(900, false, secs(15)), 900);
    }

    #[test]
    fn backoff_stays_under_the_cap() {
        let mut pace = Pace::new(10, 30);
        for _ in 0..4 {
            pace.record(UnixTime(0), false);
        }
        assert_eq!(pace.next, UnixTime(30));
    }
}
