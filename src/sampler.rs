//! The tick loop. It makes the calls that are due, diffs their answers, places started jobs and
//! publishes each tick's changes to the [`Hub`].

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::config::{Config, Groups, Names, Reach};
use crate::diff::{Known, placed};
use crate::hub::Hub;
use crate::logging::{self, Streak};
use crate::passwd;
use crate::slurm::recording::Exhausted;
use crate::slurm::{Lookup, Slurm, Source};
use crate::store::{Store, Window};
use glurmo_model::{Attempt, AttemptKey, Event, Login, Node, Outage, UnixTime, User};

/// A call slower than this counts against its kind, like a failure.
const SLOW: Duration = Duration::from_secs(2);

/// How often the log sums up how the calls fared.
const SUMMARY_S: i64 = 3600;

/// How far a call's interval can stretch under backoff.
const MAX_SLOWDOWN: u64 = 8;

/// Healthy calls in a row that halve a stretched interval again.
const HEALTHY_TO_RECOVER: u32 = 3;

/// The shortest and longest stretch of accounting read at once. A busy day on zlurm holds 27,000
/// rows, which sacct takes 15 s to print.
const BACKFILL_SPAN_S: std::ops::RangeInclusive<i64> = 900..=86_400;

/// How long to wait after a failed history read. A dead slurmdbd can hold each read for 60 s.
const BACKFILL_RETRY_S: i64 = 60;

/// The next backfill window's length, after a read of `span_s` that worked or not. On zlurm a read
/// takes about as long for a day as for 15 minutes, so only a failure shrinks the window.
fn next_span(span_s: i64, worked: bool) -> i64 {
    let span_s = if worked { span_s * 2 } else { span_s / 2 };
    span_s.clamp(*BACKFILL_SPAN_S.start(), *BACKFILL_SPAN_S.end())
}

/// Names the GPU model of each GPU node whose GRES names none, from the first of its partitions that
/// `models` lists.
fn name_gpus(nodes: &mut [Node], models: &BTreeMap<String, String>) {
    for node in nodes {
        if node.gpus > 0 && node.gpu_model.is_none() {
            node.gpu_model = node
                .partitions
                .iter()
                .find_map(|partition| models.get(&partition.0).cloned());
        }
    }
}

/// How one kind of call has fared since the sampler started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Calls {
    /// What the calls read, such as `jobs`.
    #[serde(skip)]
    name: &'static str,
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
    /// How long the slowest call took since the last hourly summary. For history, since the start.
    pub slowest_ms: Option<u64>,
    /// The run of failures the calls are in, if any.
    #[serde(skip)]
    streak: Streak,
}

impl Calls {
    /// No calls yet to what `name` names, such as `jobs`.
    fn new(name: &'static str) -> Self {
        Self {
            name,
            count: 0,
            failures: 0,
            last_ms: None,
            last_error: None,
            failing: false,
            slowest_ms: None,
            streak: Streak::default(),
        }
    }

    /// Runs one call and counts it. Also returns whether it was healthy: it worked, and within 2 s.
    async fn time<T>(&mut self, call: impl Future<Output = Result<T>>) -> (Result<T>, bool) {
        let started = Instant::now();
        let result = call.await;
        let healthy = self.tally(started.elapsed(), result.as_ref().err());
        (result, healthy)
    }

    /// Counts one call that took `took` and failed with `error`, if it failed. Returns whether it was
    /// healthy: it worked, and within 2 s.
    fn tally(&mut self, took: Duration, error: Option<&anyhow::Error>) -> bool {
        self.count += 1;
        self.last_ms = Some(u64::try_from(took.as_millis()).unwrap_or(u64::MAX));
        self.slowest_ms = self.slowest_ms.max(self.last_ms);
        self.failing = error.is_some();
        match error {
            Some(error) => {
                self.failures += 1;
                self.last_error = Some(format!("{error:#}"));
                if !error.is::<Exhausted>() {
                    self.streak.failed(self.name, error);
                }
            }
            None => self.streak.worked(self.name),
        }
        error.is_none() && took <= SLOW
    }

    /// The calls' health, made every `every_s` if on a schedule.
    fn health(&self, every_s: Option<u64>) -> CallHealth {
        CallHealth {
            call: self.name,
            every_s,
            calls: self.clone(),
        }
    }
}

/// One kind of call, and for one made on a schedule, its interval under backoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CallHealth {
    /// What the call reads, such as `jobs`.
    pub call: &'static str,
    /// Seconds between calls now, or `None` for a call made only when needed.
    pub every_s: Option<u64>,
    /// How its calls went.
    #[serde(flatten)]
    pub calls: Calls,
}

/// How every kind of call has fared since the sampler started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Health(pub Vec<CallHealth>);

/// When one kind of call is made next, how far backoff has stretched its interval, and how its calls went.
struct Pace {
    every_s: u64,
    most_s: u64,
    slowdown: u64,
    healthy: u32,
    next: UnixTime,
    calls: Calls,
}

impl Pace {
    fn new(name: &'static str, every_s: u64, most_s: u64) -> Self {
        Self {
            every_s,
            most_s,
            slowdown: 1,
            healthy: 0,
            next: UnixTime(0),
            calls: Calls::new(name),
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

    fn health(&self) -> CallHealth {
        self.calls.health(Some(self.interval_s()))
    }
}

/// Writes a tick's events to `store`. After a jobs tick, which `active` holds the queue of, it also
/// closes attempts that vanished while no listing came in, because the sampler or the controller was
/// down, and rolls up closed bins. Returns the users seen for the first time.
fn archive(
    store: &mut Store,
    now: UnixTime,
    events: &[Event],
    active: Option<&HashSet<AttemptKey>>,
    min_job_age_s: u64,
) -> Result<Vec<User>> {
    store.record(now, events)?;
    if let Some(active) = active {
        if store.sampled(now, i64::try_from(min_job_age_s)?)? {
            let closed = store.recover(active)?;
            if closed > 0 {
                tracing::info!("{closed} attempts left the queue while no listing came in");
            }
        }
        store.roll(now)?;
    }
    store.new_users()
}

/// Runs `work` on `store` on a thread meant for blocking, so a slow disk or database server holds up
/// no task, and hands the store back.
async fn blocking<T: Send + 'static>(
    mut store: Store,
    work: impl FnOnce(&mut Store) -> T + Send + 'static,
) -> (Store, T) {
    let done = tokio::task::spawn_blocking(move || {
        let answer = work(&mut store);
        (store, answer)
    })
    .await;
    done.unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
}

/// One window of accounting, read on the history task.
struct Read {
    window: Window,
    attempts: Result<Vec<Attempt>>,
    /// How the history task's calls have fared, this one included.
    calls: Calls,
}

/// The way to the task that reads accounting, so a slow slurmdbd never holds up a tick.
struct Historian {
    windows: mpsc::Sender<Window>,
    reads: mpsc::Receiver<Read>,
    /// Whether a window went out and its read has not come back.
    reading: bool,
}

impl Historian {
    /// Starts the task, which reaches Slurm as `reach` says through a connection of its own.
    fn spawn(reach: Reach) -> Self {
        let (windows, asked) = mpsc::channel(1);
        let (answers, reads) = mpsc::channel(1);
        tokio::spawn(read_history(reach, asked, answers));
        Self {
            windows,
            reads,
            reading: false,
        }
    }
}

/// Reads each window that comes through `asked` and sends it back through `answers`. It connects at the
/// first window, and again after a connection that failed.
async fn read_history(
    reach: Reach,
    mut asked: mpsc::Receiver<Window>,
    answers: mpsc::Sender<Read>,
) {
    let mut slurm: Option<Slurm> = None;
    let mut calls = Calls::new("history");
    while let Some(window) = asked.recv().await {
        let read = async {
            let slurm = match &mut slurm {
                Some(slurm) => slurm,
                None => slurm.insert(Slurm::connect(Source::live(&reach)?, None).await?),
            };
            slurm.history(window.from, window.to).await
        };
        let (attempts, _) = calls.time(read).await;
        let read = Read {
            window,
            attempts,
            calls: calls.clone(),
        };
        if answers.send(read).await.is_err() {
            return;
        }
    }
}

/// What one tick saw.
#[derive(Default)]
struct Tick {
    events: Vec<Event>,
    /// Whether the tick listed jobs.
    listed: bool,
}

/// Samples the cluster on a schedule and publishes what changed.
pub struct Sampler {
    slurm: Slurm,
    known: Known,
    jobs: Pace,
    nodes: Pace,
    partitions: Pace,
    placing: Calls,
    looking_up: Calls,
    settling: Calls,
    backfilling: Calls,
    /// Writes to the store and its upkeep.
    storing: Calls,
    /// How the calls stood at the last hourly summary.
    summarized: Health,
    /// When the next hourly summary is due, once the first tick has run.
    next_summary: Option<UnixTime>,
    /// Started attempts that still need a placement lookup, oldest first.
    lookups: VecDeque<Attempt>,
    budget: u64,
    /// Whether no jobs tick has succeeded yet, so one listing of every placement replaces the lookups.
    /// A source that places every job cheaply lists them on every jobs tick instead.
    cold: bool,
    /// Whether an outage is on.
    in_outage: bool,
    hub: Hub,
    store: Option<Store>,
    names: Names,
    /// Which groups the config shows.
    groups: Groups,
    /// GPU models by partition, for nodes whose GRES names no type.
    gpu_models: BTreeMap<String, String>,
    /// Reads accounting into the store, for a live cluster.
    historian: Option<Historian>,
    /// How much accounting the next backfill reads.
    backfill_span_s: i64,
    /// When the next history read may start, after one failed.
    backfill_after: UnixTime,
}

impl Sampler {
    /// A sampler that makes each call at the config's interval, publishes to `hub` and keeps history in
    /// `store` if there is one. It names users and GPU models as the config says.
    /// The jobs interval stays at most `MinJobAge / 2` under backoff, so no ended job leaves the queue unseen.
    pub fn new(slurm: Slurm, config: &Config, store: Option<Store>, hub: Hub) -> Self {
        let intervals = config.sample;
        let jobs_most_s = (slurm.site().min_job_age_s / 2).max(intervals.jobs_s);
        let historian =
            (store.is_some() && slurm.is_live()).then(|| Historian::spawn(config.slurm.clone()));
        Self {
            slurm,
            known: Known::default(),
            jobs: Pace::new("jobs", intervals.jobs_s, jobs_most_s),
            nodes: Pace::new("nodes", intervals.nodes_s, u64::MAX),
            partitions: Pace::new("partitions", intervals.partitions_s, u64::MAX),
            placing: Calls::new("placements"),
            looking_up: Calls::new("lookups"),
            settling: Calls::new("outcomes"),
            backfilling: Calls::new("history"),
            storing: Calls::new("store"),
            summarized: Health::default(),
            next_summary: None,
            lookups: VecDeque::new(),
            budget: intervals.lookups,
            cold: true,
            in_outage: false,
            hub,
            store,
            names: config.names,
            groups: config.groups.clone(),
            gpu_models: config.gpu_models.clone(),
            historian,
            backfill_span_s: 21_600,
            backfill_after: UnixTime(0),
        }
    }

    /// Ticks until `until`, or forever when it is `None`. A replay also stops once it runs out of answers.
    /// A failed call is logged and ends its tick early. An [`Outage`] lasts while the latest listing call failed.
    /// Every listing asks the same slurmctld, so one that answers ends it.
    pub async fn run(&mut self, until: Option<UnixTime>) {
        loop {
            let at = self.slurm.now();
            let mut tick = Tick::default();
            let failing = match self.list(at, &mut tick).await {
                Ok(()) => None,
                Err(error) if error.is::<Exhausted>() => return,
                Err(error) => {
                    tracing::debug!("tick cut short: {error:#}");
                    Some(format!("{error:#}"))
                }
            };
            let Tick { events, listed } = tick;
            let mut events = self.archive(at, events, listed).await;
            if failing.is_some() != self.in_outage {
                self.in_outage = failing.is_some();
                events.push(Event::Outage(
                    failing.map(|error| Outage { since: at, error }),
                ));
            }
            self.hub.publish(at, events);
            if listed && self.store.is_some() {
                let started = Instant::now();
                let upkeep = self.backfill(at).await;
                self.storing.tally(started.elapsed(), upkeep.as_ref().err());
            }
            self.hub.report(self.health());
            let summary = *self.next_summary.get_or_insert(UnixTime(at.0 + SUMMARY_S));
            if at >= summary {
                self.summarize();
                self.next_summary = Some(UnixTime(at.0 + SUMMARY_S));
            }
            // A replay never waits on its own, and subscribers in the same task need a turn.
            tokio::task::yield_now().await;
            let next = self.next_due();
            if until.is_some_and(|until| next > until) {
                return;
            }
            self.slurm.sleep_until(next).await;
        }
    }

    /// Logs the running and pending jobs and the nodes that cannot run jobs. Then, for each kind of call
    /// made since the last summary, how many were made and failed, the slowest, and the interval.
    fn summarize(&mut self) {
        let health = self.health();
        let calls: Vec<String> = health
            .0
            .iter()
            .filter_map(|now| {
                let before = self
                    .summarized
                    .0
                    .iter()
                    .find(|before| before.call == now.call);
                let made = now.calls.count - before.map_or(0, |before| before.calls.count);
                let failed = now.calls.failures - before.map_or(0, |before| before.calls.failures);
                let every = now
                    .every_s
                    .map_or_else(String::new, |every_s| format!(", every {every_s} s"));
                (made > 0).then(|| {
                    format!(
                        "{} {made} calls, {failed} failed, slowest {} ms{every}",
                        now.call,
                        now.calls.slowest_ms.unwrap_or(0)
                    )
                })
            })
            .collect();
        let (running, pending, nodes, unusable) = self.hub.read(|cluster| {
            let unusable = cluster
                .nodes
                .iter()
                .filter(|node| !node.is_usable())
                .count();
            (
                cluster.running.len(),
                cluster.queue.len(),
                cluster.nodes.len(),
                unusable,
            )
        });
        let calls = if calls.is_empty() {
            "no calls".to_owned()
        } else {
            calls.join("; ")
        };
        tracing::info!(
            "{running} running, {pending} pending, {unusable} of {nodes} nodes cannot run jobs. Since the last summary: {calls}"
        );
        self.summarized = health;
        for calls in [
            &mut self.jobs.calls,
            &mut self.nodes.calls,
            &mut self.partitions.calls,
            &mut self.placing,
            &mut self.looking_up,
            &mut self.settling,
            &mut self.storing,
        ] {
            calls.slowest_ms = None;
        }
    }

    fn health(&self) -> Health {
        Health(vec![
            self.jobs.health(),
            self.nodes.health(),
            self.partitions.health(),
            self.placing.health(None),
            self.looking_up.health(None),
            self.settling.health(None),
            self.backfilling.health(None),
            self.storing.health(None),
        ])
    }

    fn next_due(&self) -> UnixTime {
        [&self.jobs, &self.nodes, &self.partitions]
            .iter()
            .map(|pace| pace.next)
            .min()
            .unwrap_or_default()
    }

    /// Makes the due calls and diffs each answer as it comes into `tick`. The first failed call ends the
    /// tick, and the changes found before it still count. Jobs are diffed only together with the
    /// placements that go with them, so a job counts as gone only after a job listing that worked.
    async fn list(&mut self, now: UnixTime, tick: &mut Tick) -> Result<()> {
        if self.nodes.is_due(now) {
            let mut nodes = self.nodes.time(now, self.slurm.nodes()).await?;
            name_gpus(&mut nodes, &self.gpu_models);
            tick.events.extend(self.known.diff_nodes(nodes));
        }
        if self.partitions.is_due(now) {
            let partitions = self.partitions.time(now, self.slurm.partitions()).await?;
            tick.events.extend(self.known.diff_partitions(partitions));
        }
        if self.jobs.is_due(now) {
            let jobs = self.jobs.time(now, self.slurm.jobs()).await?;
            let placements = if self.cold || self.slurm.places_every_tick() {
                Some(self.placing.time(self.slurm.placements()).await.0?)
            } else {
                None
            };
            let changes = self.known.diff_jobs(&jobs, now);
            if placements.is_some() || self.budget > 0 {
                self.lookups
                    .extend(changes.iter().filter_map(|event| match event {
                        Event::Started { attempt } => Some(attempt.clone()),
                        _ => None,
                    }));
            }
            tick.events.extend(changes);
            match placements {
                Some(placements) => {
                    for found in placements {
                        let queued = self.lookups.iter().position(|a| a.key.job == found.job);
                        if let Some(attempt) = queued.and_then(|i| self.lookups.remove(i)) {
                            tick.events.extend(placed(&attempt, found));
                        }
                    }
                    // The listing holds every job the controller knows, so the rest have gone.
                    self.lookups.clear();
                    self.cold = false;
                }
                None => tick.events.extend(self.look_up().await),
            }
            tick.events.extend(self.settle().await);
            tick.listed = true;
        }
        Ok(())
    }

    /// Writes a tick's events to the store, and returns them with the users seen for the first time
    /// added. A failure here never drops the tick.
    async fn archive(&mut self, now: UnixTime, events: Vec<Event>, listed: bool) -> Vec<Event> {
        let Some(store) = self.store.take() else {
            return events;
        };
        let active = listed.then(|| self.known.active());
        let min_job_age_s = self.slurm.site().min_job_age_s;
        let started = Instant::now();
        let (store, (mut events, users)) = blocking(store, move |store| {
            let users = archive(store, now, &events, active.as_ref(), min_job_age_s);
            (events, users)
        })
        .await;
        self.store = Some(store);
        self.storing.tally(started.elapsed(), users.as_ref().err());
        if let Ok(users) = users
            && !users.is_empty()
        {
            let users = self.group(users).await;
            events.push(Event::Users(self.name(users).await));
        }
        events
    }

    /// Writes a window of accounting that came back into the store, sends out the next one if none is
    /// out, then runs the daily upkeep. The reads run on their own task, because one can take a minute.
    async fn backfill(&mut self, now: UnixTime) -> Result<()> {
        let Some(store) = self.store.take() else {
            return Ok(());
        };
        let mut read = None;
        let answer = match self
            .historian
            .as_mut()
            .map(|historian| historian.reads.try_recv())
        {
            Some(Ok(answer)) => Some(answer),
            Some(Err(TryRecvError::Disconnected)) => {
                tracing::error!("the history task stopped, so history waits for a restart");
                self.historian = None;
                None
            }
            Some(Err(TryRecvError::Empty)) | None => None,
        };
        if let Some(answer) = answer {
            if let Some(historian) = &mut self.historian {
                historian.reading = false;
            }
            self.backfilling = answer.calls;
            self.backfill_span_s = next_span(self.backfill_span_s, answer.attempts.is_ok());
            match answer.attempts {
                Ok(attempts) => read = Some((answer.window, attempts)),
                Err(error) => {
                    let Window { from, to, .. } = answer.window;
                    tracing::debug!(
                        "cannot read history from {} to {}: {error:#}",
                        logging::local(from),
                        logging::local(to)
                    );
                    self.backfill_after = UnixTime(now.0 + BACKFILL_RETRY_S);
                }
            }
        }
        let ask = self
            .historian
            .as_ref()
            .is_some_and(|historian| !historian.reading)
            && now >= self.backfill_after;
        let active = read.as_ref().map(|_| self.known.active());
        let min_job_age_s = i64::try_from(self.slurm.site().min_job_age_s)?;
        let span_s = self.backfill_span_s;
        let (store, next) = blocking(store, move |store| {
            if let (Some((window, attempts)), Some(active)) = (read, active) {
                store.backfill(window, &attempts, &active, now)?;
            }
            let next = if ask {
                store.next_backfill(now, min_job_age_s, span_s)?
            } else {
                None
            };
            store.maintain(now)?;
            Ok::<_, anyhow::Error>(next)
        })
        .await;
        self.store = Some(store);
        if let Some(window) = next?
            && let Some(historian) = &mut self.historian
        {
            historian.reading = historian.windows.try_send(window).is_ok();
        }
        Ok(())
    }

    /// Gives new users the groups the config shows. A live cluster's users are looked
    /// up in this host's account database, and what it says is stored. A replay's logins are its
    /// recording's, so they keep what the store holds.
    async fn group(&mut self, mut users: Vec<User>) -> Vec<User> {
        if self.slurm.is_live() {
            let logins: Vec<Login> = users.iter().map(|user| user.login.clone()).collect();
            match passwd::groups(&logins).await {
                Ok(mut groups) => {
                    for user in &mut users {
                        if let Some(member_of) = groups.get_mut(&user.login) {
                            user.groups.clone_from(member_of);
                        }
                    }
                    if let Some(store) = self.store.take() {
                        let started = Instant::now();
                        let (store, outcome) =
                            blocking(store, move |store| store.group_users(&groups)).await;
                        self.store = Some(store);
                        self.storing
                            .tally(started.elapsed(), outcome.as_ref().err());
                    }
                }
                Err(error) => tracing::warn!("cannot look up user groups: {error:#}"),
            }
        }
        for user in &mut users {
            user.groups.retain(|group| self.groups.shows(group));
        }
        users
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
                tracing::warn!("cannot look up full names: {error:#}");
                return users;
            }
        };
        if let Some(store) = self.store.take() {
            let stored = names.clone();
            let started = Instant::now();
            let (store, outcome) = blocking(store, move |store| store.name_users(&stored)).await;
            self.store = Some(store);
            self.storing
                .tally(started.elapsed(), outcome.as_ref().err());
        }
        for user in &mut users {
            if let Some(name) = names.get(&user.login) {
                user.name = Some(name.clone());
            }
        }
        users
    }

    /// Looks up queued placements, oldest first, up to the budget. A failed lookup goes to the back of
    /// the queue and ends the pass, so one that keeps failing cannot hold up the rest.
    async fn look_up(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for _ in 0..self.budget {
            let Some(attempt) = self.lookups.pop_front() else {
                break;
            };
            let lookup = self.slurm.lookup(&attempt);
            match self.looking_up.time(lookup).await.0 {
                Ok(Lookup::Found(found)) => events.extend(placed(&attempt, found)),
                Ok(Lookup::Gone) => {}
                Err(error) => {
                    tracing::debug!("lookup of job {} failed: {error:#}", attempt.key.job.0);
                    self.lookups.push_back(attempt);
                    break;
                }
            }
        }
        events
    }

    /// Asks accounting how jobs that left the queue unseen ended. A failure leaves them for the next tick.
    async fn settle(&mut self) -> Vec<Event> {
        let jobs = self.known.unsettled();
        if jobs.is_empty() {
            return Vec::new();
        }
        match self.settling.time(self.slurm.outcomes(&jobs)).await.0 {
            Ok(outcomes) => self.known.settle(&outcomes),
            Err(error) => {
                tracing::debug!("cannot read how {} jobs ended: {error:#}", jobs.len());
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use super::*;
    use crate::fixture::{
        StoreHome, busy_hub, busy_quarter_hour, busy_quarter_hour_with, zlurm_config,
    };
    use crate::slurm::recording::Call;
    use crate::store::connection::params;
    use glurmo_model::{AttemptKey, Cluster, Delta, Owner};

    /// One line per delta: its time, how many events of each kind it holds, and how the cluster looks after it.
    fn summary(delta: &Delta, start: UnixTime, cluster: &Cluster) -> String {
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        for event in &delta.events {
            let kind = match event {
                Event::Started { .. } => "started".to_owned(),
                Event::Ended { state, .. } => format!("ended:{state:?}"),
                Event::Withdrawn { .. } => "withdrawn".to_owned(),
                Event::Settled { state, .. } => format!("settled:{state:?}"),
                Event::Updated { .. } => "updated".to_owned(),
                Event::Placed { .. } => "placed".to_owned(),
                Event::Queued(attempts) => format!("queued:{}", attempts.len()),
                Event::Dequeued(keys) => format!("dequeued:{}", keys.len()),
                Event::Nodes(_) => "nodes".to_owned(),
                Event::NodesChanged(nodes) => format!("changed nodes:{}", nodes.len()),
                Event::Partitions(_) => "partitions".to_owned(),
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
        assert_store_agrees_with_the_deltas(busy_quarter_hour().await, 0, None).await;
    }

    /// The busy quarter hour, with a controller outage longer than `MinJobAge` in it.
    async fn busy_quarter_hour_with_an_outage() -> Slurm {
        let mut listings = 0;
        busy_quarter_hour_with(|exchange| {
            if exchange.call != Call::Jobs {
                return;
            }
            listings += 1;
            // Backoff spaces these seven failures over more than MinJobAge.
            if (25..32).contains(&listings) {
                exchange.answer = Err("slurmctld did not answer: connection refused".into());
            }
        })
        .await
    }

    #[tokio::test]
    async fn a_run_of_failed_calls_logs_where_it_starts_and_ends() {
        let slurm = busy_quarter_hour_with_an_outage().await;
        let hub = Hub::new(slurm.site().clone());
        let (subscriber, captured) = crate::logging::capturing("info");
        let _logging = tracing::subscriber::set_default(subscriber);
        Sampler::new(slurm, &zlurm_config(), None, hub)
            .run(None)
            .await;
        let text = captured.text();
        let lines: Vec<&str> = text.lines().collect();
        let failing: Vec<&&str> = lines.iter().filter(|line| line.contains("WARN")).collect();
        assert_eq!(failing.len(), 1, "{text}");
        assert!(
            failing[0].contains("jobs failing: slurmctld did not answer"),
            "{text}"
        );
        let recovered: Vec<&&str> = lines
            .iter()
            .filter(|line| line.contains("jobs answers again"))
            .collect();
        assert_eq!(recovered.len(), 1, "{text}");
        assert!(recovered[0].contains("7 of 10 calls failed"), "{text}");
    }

    #[tokio::test]
    async fn the_summary_counts_the_calls_since_the_last_one_and_the_queue() {
        let slurm = busy_quarter_hour().await;
        let hub = Hub::new(slurm.site().clone());
        let mut sampler = Sampler::new(slurm, &zlurm_config(), None, hub);
        sampler.run(None).await;
        let (subscriber, captured) = crate::logging::capturing("info");
        let _logging = tracing::subscriber::set_default(subscriber);
        sampler.summarize();
        sampler.summarize();
        let text = captured.text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        let jobs = sampler.jobs.calls.count;
        assert!(lines[0].contains(&format!("jobs {jobs} calls, ")), "{text}");
        assert!(
            lines[0].contains(" running, ") && lines[0].contains(" pending, "),
            "{text}"
        );
        assert!(
            lines[1].ends_with("Since the last summary: no calls"),
            "{text}"
        );
    }

    /// When the store's coverage ends, in seconds since the epoch.
    fn complete_to(home: &StoreHome) -> i64 {
        home.read()
            .query_row(
                "SELECT value FROM meta WHERE key = 'complete_to'",
                params![],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// A history task that answers each window with no records, a tick after it went out.
    fn empty_accounting() -> Historian {
        let (windows, mut asked) = mpsc::channel(1);
        let (answers, reads) = mpsc::channel(1);
        tokio::spawn(async move {
            while let Some(window) = asked.recv().await {
                let read = Read {
                    window,
                    attempts: Ok(Vec::new()),
                    calls: Calls::new("history"),
                };
                if answers.send(read).await.is_err() {
                    return;
                }
            }
        });
        Historian {
            windows,
            reads,
            reading: false,
        }
    }

    #[tokio::test]
    async fn store_agrees_with_the_deltas_after_a_controller_outage() {
        let slurm = busy_quarter_hour_with_an_outage().await;
        let (home, deltas) = assert_store_agrees_with_the_deltas(slurm, 0, None).await;
        let outages = outages(&deltas);
        assert!(
            outages[0].is_some_and(|error| error.contains("slurmctld did not answer")),
            "{outages:?}"
        );
        assert_eq!(
            outages.last(),
            Some(&None),
            "node listings answer in between"
        );
        let last = deltas.last().unwrap().at;
        assert!(
            last.0 - complete_to(&home) > 300,
            "the store waits at the outage for accounting, which this replay lacks"
        );
    }

    #[tokio::test]
    async fn accounting_read_on_its_own_task_closes_an_outage_gap() {
        let slurm = busy_quarter_hour_with_an_outage().await;
        let (home, deltas) =
            assert_store_agrees_with_the_deltas(slurm, 0, Some(empty_accounting())).await;
        let last = deltas.last().unwrap().at;
        assert!(last.0 - complete_to(&home) <= 30, "the gap closed");
    }

    #[tokio::test]
    async fn a_history_read_that_never_ends_holds_up_no_tick() {
        let (windows, mut asked) = mpsc::channel(1);
        let (_answers, reads) = mpsc::channel(1);
        let hung = Historian {
            windows,
            reads,
            reading: false,
        };
        let replay = assert_store_agrees_with_the_deltas(busy_quarter_hour().await, 1, Some(hung));
        let (_, deltas) = tokio::time::timeout(Duration::from_secs(60), replay)
            .await
            .expect("the replay ran to its end");
        let unstored = busy_hub().await.deltas();
        assert_eq!(deltas.len(), unstored.len());
        assert!(asked.try_recv().is_ok(), "one window went out");
        assert!(asked.try_recv().is_err(), "and no more while it was out");
    }

    /// Samples `slurm` into a new store that backfills `backfill_days` through `historian`, then checks
    /// the store against the deltas: open attempts that started are the running ones, every end landed,
    /// and open pending attempts hold the queue's tasks. Returns the store's home and the deltas.
    async fn assert_store_agrees_with_the_deltas(
        slurm: Slurm,
        backfill_days: u32,
        historian: Option<Historian>,
    ) -> (StoreHome, Vec<std::sync::Arc<Delta>>) {
        let home = StoreHome::new();
        let store = home.open(backfill_days);
        let hub = Hub::new(slurm.site().clone());
        let mut sampler = Sampler::new(slurm, &zlurm_config(), Some(store), hub.clone());
        sampler.historian = historian;
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

        let db = home.read();
        let keys = |sql: &str| -> HashSet<(u32, i64)> {
            db.query_map(sql, params![], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .into_iter()
                .collect()
        };
        assert_eq!(
            keys(r#"SELECT job, submit FROM attempts WHERE start IS NOT NULL AND "end" IS NULL"#),
            running
        );
        assert!(ended.is_subset(&keys(
            r#"SELECT job, submit FROM attempts WHERE "end" IS NOT NULL"#
        )));
        let waiting: u32 = db
            .query_row(
                r#"SELECT CAST(sum(tasks) AS BIGINT) FROM attempts
                   WHERE start IS NULL AND "end" IS NULL"#,
                params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(waiting, pending);
        let bins: u32 = db
            .query_row("SELECT count(DISTINCT bin) FROM usage", params![], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(bins >= 2, "{bins} bins rolled up");
        let users: usize = db
            .query_row("SELECT count(*) FROM users", params![], |row| row.get(0))
            .unwrap();
        assert_eq!(users, cluster.users.len());
        drop(db);
        (home, deltas)
    }

    #[tokio::test]
    async fn a_store_that_waits_on_a_lock_holds_up_no_other_task() {
        let home = StoreHome::sqlite();
        let store = home.open(0);
        let locker = rusqlite::Connection::open(home.path()).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let slurm = busy_quarter_hour().await;
        let hub = Hub::new(slurm.site().clone());
        let mut sampler = Sampler::new(slurm, &zlurm_config(), Some(store), hub);
        let started = Instant::now();
        let other_task = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let waited = started.elapsed();
            locker.execute_batch("COMMIT").unwrap();
            waited
        };
        let ((), waited) = tokio::join!(sampler.run(None), other_task);
        assert!(waited < Duration::from_secs(2), "it waited {waited:?}");
    }

    #[tokio::test]
    async fn a_second_run_on_a_store_names_every_user_without_accounting() {
        let home = StoreHome::new();
        let mut hub = None;
        for _ in 0..2 {
            let store = home.open(0);
            let slurm = busy_quarter_hour().await;
            let run = Hub::new(slurm.site().clone());
            Sampler::new(slurm, &zlurm_config(), Some(store), run.clone())
                .run(None)
                .await;
            hub = Some(run);
        }
        let stored: usize = home
            .read()
            .query_row("SELECT count(*) FROM users", params![], |row| row.get(0))
            .unwrap();
        assert!(stored > 0);
        assert_eq!(hub.unwrap().read(|cluster| cluster.users.len()), stored);
    }

    #[tokio::test]
    async fn attempts_stored_before_the_first_node_listing_get_their_cores_counted_again() {
        let core_s_by_bin = |home: &StoreHome| -> BTreeMap<i64, i64> {
            let db = home.read();
            let miscounted: u32 = db
                .query_row(
                    "SELECT count(*) FROM attempts
                     WHERE nodes != '[]' AND coalesce(cores, 0) != (cpus + 1) / 2",
                    params![],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                miscounted, 0,
                "every node in the fixture runs 2 threads per core"
            );
            db.query_map(
                "SELECT bin, CAST(sum(core_s) AS BIGINT) FROM usage GROUP BY bin",
                params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
            .into_iter()
            .collect()
        };
        let (clean, _) =
            assert_store_agrees_with_the_deltas(busy_quarter_hour().await, 0, None).await;
        let mut failed = false;
        let slurm = busy_quarter_hour_with(|exchange| {
            if exchange.call == Call::Nodes && !failed {
                failed = true;
                exchange.answer = Err("slurmctld did not answer".into());
            }
        })
        .await;
        let (late, _) = assert_store_agrees_with_the_deltas(slurm, 0, None).await;
        assert_eq!(core_s_by_bin(&late), core_s_by_bin(&clean));
    }

    #[tokio::test]
    async fn no_lookup_budget_queues_no_lookups() {
        let slurm = busy_quarter_hour().await;
        let config =
            crate::config::from_ron(r#"(sample: (lookups: 0), gpu_models: {"gpu": "V100"})"#)
                .unwrap();
        let hub = Hub::new(slurm.site().clone());
        let mut sampler = Sampler::new(slurm, &config, None, hub);
        sampler.run(None).await;
        assert_eq!(sampler.lookups.len(), 0);
    }

    /// The deltas of the busy quarter hour, with the first answer to `failing` replaced by an error.
    async fn deltas_when_the_first_call_fails(failing: Call) -> Vec<std::sync::Arc<Delta>> {
        let mut failed = false;
        let slurm = busy_quarter_hour_with(|exchange| {
            if exchange.call == failing && !failed {
                failed = true;
                exchange.answer = Err("slurmctld did not answer".into());
            }
        })
        .await;
        let hub = Hub::new(slurm.site().clone());
        Sampler::new(slurm, &zlurm_config(), None, hub.clone())
            .run(None)
            .await;
        hub.deltas()
    }

    /// The outages that `deltas` begin, as their errors, and end, as `None`.
    fn outages(deltas: &[std::sync::Arc<Delta>]) -> Vec<Option<&str>> {
        deltas
            .iter()
            .flat_map(|delta| &delta.events)
            .filter_map(|event| match event {
                Event::Outage(outage) => Some(outage.as_ref().map(|outage| outage.error.as_str())),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_failed_job_listing_keeps_the_node_and_partition_listings_before_it() {
        let deltas = deltas_when_the_first_call_fails(Call::Jobs).await;
        let kinds: HashSet<&str> = deltas[0]
            .events
            .iter()
            .map(|event| match event {
                Event::Nodes(_) => "nodes",
                Event::Partitions(_) => "partitions",
                Event::Started { .. } => "started",
                Event::Outage(_) => "outage",
                _ => "other",
            })
            .collect();
        assert!(kinds.contains("nodes"), "{kinds:?}");
        assert!(kinds.contains("partitions"), "{kinds:?}");
        assert!(kinds.contains("outage"), "{kinds:?}");
        assert!(!kinds.contains("started"), "{kinds:?}");
    }

    #[tokio::test]
    async fn a_healthy_job_listing_ends_an_outage_that_a_partition_listing_began() {
        let deltas = deltas_when_the_first_call_fails(Call::Partitions).await;
        assert_eq!(
            outages(&deltas),
            [Some("slurmctld did not answer"), None],
            "backoff puts the next partition listing past the replay"
        );
    }

    #[tokio::test]
    async fn the_sampler_never_asks_for_scheduler_stats() {
        let slurm = busy_quarter_hour_with(|exchange| {
            if exchange.call == Call::Stats {
                exchange.answer = Err("asked for stats".into());
            }
        })
        .await;
        let hub = Hub::new(slurm.site().clone());
        Sampler::new(slurm, &zlurm_config(), None, hub.clone())
            .run(None)
            .await;
        let outages = hub
            .deltas()
            .iter()
            .flat_map(|delta| delta.events.clone())
            .filter(|event| matches!(event, Event::Outage(_)))
            .count();
        assert_eq!(outages, 0);
    }

    #[tokio::test]
    async fn a_source_that_lists_placements_each_tick_places_starts_without_lookups() {
        use crate::slurm::Source;
        use crate::slurm::recording::{Answer, Exchange, Replay, read};
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/slurm-23.11/quiet.jsonl");
        let mut exchanges = read(&path).unwrap();
        let answer = |call: &Call| {
            exchanges
                .iter()
                .find(|exchange| exchange.call == *call)
                .and_then(|exchange| exchange.answer.clone().ok())
                .unwrap()
        };
        let (Answer::Jobs(mut jobs), Answer::Placements(mut placements)) =
            (answer(&Call::Jobs), answer(&Call::Placements))
        else {
            panic!("the fixture lacks jobs or placements");
        };
        let at = exchanges.last().unwrap().at.0 + 10;
        let running = jobs
            .iter()
            .position(|job| job.state == glurmo_model::JobState::Running)
            .unwrap();
        let mut started = jobs[running].clone();
        started.key.job = glurmo_model::JobId(99_999_999);
        started.start = Some(UnixTime(at - 5));
        let mut found = placements
            .iter()
            .find(|placed| placed.job == jobs[running].key.job)
            .unwrap()
            .clone();
        found.job = started.key.job;
        found.start = started.start;
        jobs.push(started.clone());
        placements.push(found);
        for (call, answer) in [
            (Call::Jobs, Answer::Jobs(jobs)),
            (Call::Placements, Answer::Placements(placements)),
        ] {
            exchanges.push(Exchange {
                at: UnixTime(at),
                call,
                answer: Ok(answer),
            });
        }
        let slurm = Slurm::connect(Source::Replay(Replay::new(exchanges)), None)
            .await
            .unwrap();
        assert!(slurm.places_every_tick());
        let hub = Hub::new(slurm.site().clone());
        Sampler::new(slurm, &zlurm_config(), None, hub.clone())
            .run(None)
            .await;
        let placed = hub
            .deltas()
            .iter()
            .flat_map(|delta| &delta.events)
            .any(|event| matches!(event, Event::Placed { key, .. } if *key == started.key));
        assert!(placed, "the start was not placed from the listing");
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
        let mut pace = Pace::new("jobs", 10, 150);
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
    fn backfill_window_doubles_after_a_read_that_worked_and_halves_after_one_that_failed() {
        assert_eq!(next_span(21_600, true), 43_200);
        assert_eq!(next_span(86_400, true), 86_400);
        assert_eq!(next_span(21_600, false), 10_800);
        assert_eq!(next_span(900, false), 900);
    }

    #[tokio::test]
    async fn gpu_models_come_from_the_config_when_gres_names_none() {
        let hub = busy_hub().await;
        let models: BTreeMap<String, Option<String>> = hub.read(|cluster| {
            cluster
                .nodes
                .iter()
                .filter(|node| node.gpus > 0)
                .map(|node| (node.name.0.clone(), node.gpu_model.clone()))
                .collect()
        });
        assert_eq!(models["z018"].as_deref(), Some("V100"));
        assert_eq!(models["z028"].as_deref(), Some("H200"));
        assert_eq!(models["js029"], None, "its partition is not in the config");
    }

    #[test]
    fn backoff_stays_under_the_cap() {
        let mut pace = Pace::new("jobs", 10, 30);
        for _ in 0..4 {
            pace.record(UnixTime(0), false);
        }
        assert_eq!(pace.next, UnixTime(30));
    }
}
