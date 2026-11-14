//! The tick loop. It runs the commands that are due, diffs their output, looks up placements and
//! publishes each tick's changes to the [`Hub`].

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::broadcast;

use crate::diff::{Known, placed};
use crate::model::{Attempt, Delta, Event, UnixTime};
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

/// When one command runs next, and how far backoff has stretched its interval.
struct Pace {
    every_s: u64,
    most_s: u64,
    slowdown: u64,
    healthy: u32,
    next: UnixTime,
}

impl Pace {
    fn new(every_s: u64, most_s: u64) -> Self {
        Self {
            every_s,
            most_s,
            slowdown: 1,
            healthy: 0,
            next: UnixTime(0),
        }
    }

    fn is_due(&self, now: UnixTime) -> bool {
        now >= self.next
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
        let interval = (self.every_s * self.slowdown).min(self.most_s);
        self.next = UnixTime(now.0 + i64::try_from(interval).unwrap_or(i64::MAX));
    }

    /// Runs one call and paces the next by how it went.
    async fn time<T>(&mut self, now: UnixTime, call: impl Future<Output = Result<T>>) -> Result<T> {
        let started = Instant::now();
        let result = call.await;
        self.record(now, result.is_ok() && started.elapsed() <= SLOW);
        result
    }
}

/// Numbers each tick's changes and sends them to every subscriber.
pub struct Hub {
    seq: u64,
    sender: broadcast::Sender<Arc<Delta>>,
}

impl Hub {
    fn new() -> Self {
        Self {
            seq: 0,
            sender: broadcast::channel(1024).0,
        }
    }

    /// Sends a tick's events as the next delta. A tick that changed nothing sends nothing.
    fn publish(&mut self, at: UnixTime, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        self.seq += 1;
        let delta = Delta {
            seq: self.seq,
            at,
            events,
        };
        // Nobody listening is not an error.
        let _ = self.sender.send(Arc::new(delta));
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
    /// Started attempts that still need a placement lookup, oldest first.
    lookups: VecDeque<Attempt>,
    budget: u64,
    /// Whether no jobs tick has succeeded yet, so one listing of every placement replaces the lookups.
    cold: bool,
    hub: Hub,
    store: Option<Store>,
    /// How much accounting the next backfill reads.
    backfill_span_s: i64,
}

impl Sampler {
    /// A sampler that runs each command at its interval, and keeps history in `store` if there is one.
    /// The jobs interval stays at most `MinJobAge / 2` under backoff, so no ended job leaves squeue unseen.
    pub fn new(slurm: Slurm, intervals: Intervals, store: Option<Store>) -> Self {
        let jobs_most_s = (slurm.site().min_job_age_s / 2).max(intervals.jobs_s);
        Self {
            slurm,
            known: Known::default(),
            jobs: Pace::new(intervals.jobs_s, jobs_most_s),
            nodes: Pace::new(intervals.nodes_s, u64::MAX),
            partitions: Pace::new(intervals.partitions_s, u64::MAX),
            stats: Pace::new(intervals.stats_s, u64::MAX),
            lookups: VecDeque::new(),
            budget: intervals.lookups,
            cold: true,
            hub: Hub::new(),
            store,
            backfill_span_s: 21_600,
        }
    }

    /// A stream of every delta published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Delta>> {
        self.hub.sender.subscribe()
    }

    /// Ticks until `until`, or forever when it is `None`. A replay also stops once it runs out of answers.
    /// A failed tick is logged and skipped.
    pub async fn run(&mut self, until: Option<UnixTime>) {
        loop {
            let at = self.slurm.now();
            match self.tick(at).await {
                Ok(events) => self.hub.publish(at, events),
                Err(error) if error.is::<Exhausted>() => return,
                Err(error) => eprintln!("tick at {} skipped: {error:#}", at.0),
            }
            let next = self.next_due();
            if until.is_some_and(|until| next > until) {
                return;
            }
            self.slurm.sleep_until(next).await;
        }
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
            Some(self.slurm.placements(nodes).await?)
        } else {
            None
        };
        let stats = if self.stats.is_due(now) {
            Some(self.stats.time(now, self.slurm.stats()).await?)
        } else {
            None
        };

        let first = self.cold;
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
        if let Err(error) = self.archive(now, &events, listed, first).await {
            eprintln!("store at {}: {error:#}", now.0);
        }
        Ok(events)
    }

    /// Writes a tick's events to the store. After a jobs tick it also closes attempts that vanished
    /// while the sampler was down, rolls up closed bins, reads one window of accounting and runs the
    /// daily upkeep. A failure here never drops the tick.
    async fn archive(
        &mut self,
        now: UnixTime,
        events: &[Event],
        listed: bool,
        first: bool,
    ) -> Result<()> {
        let Some(store) = &mut self.store else {
            return Ok(());
        };
        store.record(now, events)?;
        if !listed {
            return Ok(());
        }
        let active = self.known.active();
        if first {
            let closed = store.recover(&active)?;
            if closed > 0 {
                eprintln!("{closed} attempts left the queue while the sampler was down");
            }
        }
        store.sampled(now)?;
        store.roll(now)?;
        let overlap_s = i64::try_from(self.slurm.site().min_job_age_s)?;
        if let Some(window) = store.next_backfill(now, overlap_s, self.backfill_span_s)? {
            let started = Instant::now();
            let attempts = self.slurm.history(window.from, window.to).await;
            self.backfill_span_s =
                next_span(self.backfill_span_s, attempts.is_ok(), started.elapsed());
            store.backfill(window, &attempts?, &active, now)?;
        }
        store.maintain(now)
    }

    /// Looks up queued placements, oldest first, up to the budget. A failed lookup waits for the next tick.
    async fn look_up(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for _ in 0..self.budget {
            let Some(attempt) = self.lookups.pop_front() else {
                break;
            };
            match self.slurm.lookup(&attempt, self.known.nodes()).await {
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
        match self.slurm.outcomes(&jobs).await {
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
    use crate::model::{AttemptKey, Cluster, Owner};
    use crate::slurm::source::{Replay, Source};
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
                Event::Queue(queue) => format!("queue:{}", queue.len()),
                Event::Nodes(_) => "nodes".to_owned(),
                Event::Partitions(_) => "partitions".to_owned(),
                Event::Stats(_) => "stats".to_owned(),
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

    /// Connects to a replay of a pseudonymized busy quarter hour on zlurm.
    async fn busy_quarter_hour() -> Slurm {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/slurm-23.11/busy-15m.jsonl.zst");
        let mut compressed = std::fs::File::open(path).unwrap();
        let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut compressed).unwrap();
        let mut recording = tempfile::NamedTempFile::new().unwrap();
        std::io::copy(&mut decoder, &mut recording).unwrap();
        Slurm::connect(Source::Replay(Replay::open(recording.path()).unwrap()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn store_agrees_with_the_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtviz.sqlite");
        let settings = Settings {
            backfill_days: 0,
            keep_days: None,
            backup: dir.path().join("backup.sqlite"),
        };
        let store = Store::open(&path, settings).unwrap();
        let mut sampler =
            Sampler::new(busy_quarter_hour().await, Intervals::default(), Some(store));
        let mut deltas = sampler.subscribe();
        sampler.run(None).await;

        let mut cluster = Cluster::default();
        let mut ended = HashSet::new();
        while let Ok(delta) = deltas.try_recv() {
            for event in &delta.events {
                if let Event::Ended { key, .. } = event {
                    ended.insert((key.job.0, key.submit.0));
                }
            }
            cluster.apply(&delta);
        }
        let running: HashSet<(u32, i64)> = cluster
            .running
            .keys()
            .map(|key| (key.job.0, key.submit.0))
            .collect();
        let pending: u32 = cluster
            .queue
            .iter()
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
    }

    #[tokio::test]
    async fn replay_of_a_busy_quarter_hour() {
        let mut sampler = Sampler::new(busy_quarter_hour().await, Intervals::default(), None);
        let mut deltas = sampler.subscribe();
        sampler.run(None).await;

        let mut cluster = Cluster::default();
        let mut lines = Vec::new();
        let mut unplaced: HashSet<AttemptKey> = HashSet::new();
        let mut first_at = None;
        while let Ok(delta) = deltas.try_recv() {
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
