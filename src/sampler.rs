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

/// A call slower than this counts against its command, like a failure.
const SLOW: Duration = Duration::from_secs(2);

/// How far a command's interval can stretch under backoff.
const MAX_SLOWDOWN: u64 = 8;

/// Healthy calls in a row that halve a stretched interval again.
const HEALTHY_TO_RECOVER: u32 = 3;

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
}

impl Sampler {
    /// A sampler that runs each command at its interval.
    /// The jobs interval stays at most `MinJobAge / 2` under backoff, so no ended job leaves squeue unseen.
    pub fn new(slurm: Slurm, intervals: Intervals) -> Self {
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
        Ok(events)
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
    use super::*;

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
    fn backoff_stays_under_the_cap() {
        let mut pace = Pace::new(10, 30);
        for _ in 0..4 {
            pace.record(UnixTime(0), false);
        }
        assert_eq!(pace.next, UnixTime(30));
    }
}
