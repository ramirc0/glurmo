//! The only code that knows Slurm: its protocol, its commands, and their quirks.

pub mod cli;
pub mod fields;
pub mod hostlist;
pub mod probe;
pub mod recording;
pub mod sacct;
pub mod scontrol;
pub mod sdiag;
pub mod sinfo;
pub mod squeue;
pub mod wire;

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::Reach;
use cli::Cli;
use glurmo_model::{Attempt, JobId, JobState, Node, NodeName, Partition, Stats, UnixTime};
use recording::{Answer, Call, Recorder, Replay};
use sacct::Outcome;
use scontrol::Placed;
use wire::Wire;

/// A Slurm release, such as 23.11.4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl FromStr for Version {
    type Err = anyhow::Error;

    /// Parses `23.11.4`. Suffixes such as `-0rc1` are ignored.
    fn from_str(text: &str) -> Result<Self> {
        let mut parts = text.split('.').map(|part| {
            let digits = part
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(part.len());
            part[..digits].parse::<u32>()
        });
        match (parts.next(), parts.next(), parts.next()) {
            (Some(Ok(major)), Some(Ok(minor)), Some(Ok(micro))) => Ok(Self(major, minor, micro)),
            _ => bail!("bad Slurm version {text:?}"),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The controller's per-user RPC rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Tokens a user can save up.
    pub bucket_size: u32,
    /// Tokens added each period.
    pub refill_rate: u32,
    /// The refill period.
    pub refill_period_s: u32,
}

/// Reads the `rl_*` settings, which matter only once `rl_enable` is set. Missing ones take Slurm's defaults.
pub(crate) fn rate_limit(parameters: &str) -> Result<Option<RateLimit>> {
    let parameters: Vec<&str> = parameters.split(',').collect();
    if !parameters.contains(&"rl_enable") {
        return Ok(None);
    }
    let mut limit = RateLimit {
        bucket_size: 30,
        refill_rate: 2,
        refill_period_s: 1,
    };
    for parameter in parameters {
        match parameter.split_once('=') {
            Some(("rl_bucket_size", value)) => limit.bucket_size = fields::integer(value)?,
            Some(("rl_refill_rate", value)) => limit.refill_rate = fields::integer(value)?,
            Some(("rl_refill_period", value)) => limit.refill_period_s = fields::integer(value)?,
            _ => {}
        }
    }
    Ok(Some(limit))
}

/// Facts about the site, read from the controller's configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    /// The cluster name.
    pub cluster: String,
    /// The controller's Slurm version.
    pub version: Version,
    /// How long ended jobs stay in the controller's job list.
    pub min_job_age_s: u64,
    /// What the controller hides from other users, such as `jobs` or `nodes`.
    pub private_data: Vec<String>,
    /// The RPC rate limit, if the controller enforces one.
    pub rate_limit: Option<RateLimit>,
}

impl Site {
    /// zlurm's facts, for tests.
    #[cfg(test)]
    pub fn zlurm() -> Self {
        Self {
            cluster: "zlurm".into(),
            version: Version(23, 11, 4),
            min_job_age_s: 300,
            private_data: Vec::new(),
            rate_limit: None,
        }
    }

    /// Whether the controller hides other users' jobs.
    pub fn hides_jobs(&self) -> bool {
        self.private_data.iter().any(|data| data == "jobs")
    }
}

/// How often to make each call, and how many placement lookups to make per jobs tick.
/// In the config, intervals read like `jobs: "10s"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Intervals {
    /// Jobs.
    #[serde(rename = "jobs", with = "crate::config::seconds")]
    pub jobs_s: u64,
    /// Nodes.
    #[serde(rename = "nodes", with = "crate::config::seconds")]
    pub nodes_s: u64,
    /// Partitions.
    #[serde(rename = "partitions", with = "crate::config::seconds")]
    pub partitions_s: u64,
    /// Scheduler stats.
    #[serde(rename = "stats", with = "crate::config::seconds")]
    pub stats_s: u64,
    /// Placement lookups per jobs tick.
    pub lookups: u64,
}

impl Default for Intervals {
    fn default() -> Self {
        Self {
            jobs_s: 10,
            nodes_s: 30,
            partitions_s: 600,
            stats_s: 60,
            lookups: 25,
        }
    }
}

/// The answer to a placement lookup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Lookup {
    /// The job and its placement.
    Found(Placed),
    /// The controller no longer knows the job.
    Gone,
}

/// Where answers come from.
pub enum Source {
    /// Slurm's protocol, spoken to slurmctld and slurmdbd.
    Wire(Wire),
    /// Slurm's commands, run on this host.
    Cli(Cli),
    /// A recording.
    Replay(Replay),
}

impl Source {
    /// The live cluster, reached as the config says.
    ///
    /// # Errors
    ///
    /// Fails when the protocol backend finds no controller.
    pub fn live(reach: &Reach) -> Result<Self> {
        Ok(match reach.backend {
            Backend::Wire if reach.controller.is_empty() => {
                Self::Wire(Wire::new(wire::controllers()?)?)
            }
            Backend::Wire => Self::Wire(Wire::new(reach.controller.clone())?),
            Backend::Cli => Self::Cli(Cli::default()),
        })
    }
}

/// How glurmo reaches a live cluster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// Slurm 23.11's protocol, which controllers through 25.05 accept.
    #[default]
    Wire,
    /// Slurm's commands, for clusters the protocol cannot reach.
    Cli,
}

/// Asks Slurm through a [`Source`], recording each call and answer if asked to.
pub struct Slurm {
    source: Source,
    recorder: Option<Recorder>,
    site: Site,
}

impl Slurm {
    /// Reads the site's configuration through `source`.
    ///
    /// # Errors
    ///
    /// Fails when the configuration cannot be read or lacks a needed setting.
    pub async fn connect(source: Source, recorder: Option<Recorder>) -> Result<Self> {
        let mut slurm = Self {
            source,
            recorder,
            site: Site {
                cluster: String::new(),
                version: Version(0, 0, 0),
                min_job_age_s: 0,
                private_data: Vec::new(),
                rate_limit: None,
            },
        };
        let Answer::Config(site) = slurm.ask(Call::Config).await? else {
            bail!("the configuration call gave another answer");
        };
        slurm.site = site;
        Ok(slurm)
    }

    /// The site facts read at connection.
    pub fn site(&self) -> &Site {
        &self.site
    }

    /// The current time, which a replay drives.
    pub fn now(&self) -> UnixTime {
        match &self.source {
            Source::Replay(replay) => replay.now(),
            Source::Wire(_) | Source::Cli(_) => UnixTime::now(),
        }
    }

    /// Waits until `time`. A replay moves its clock there at once.
    pub async fn sleep_until(&mut self, time: UnixTime) {
        match &mut self.source {
            Source::Replay(replay) => replay.sleep_until(time).await,
            Source::Wire(_) | Source::Cli(_) => {
                let wait = u64::try_from(time.0 - UnixTime::now().0).unwrap_or(0);
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
        }
    }

    /// Whether listing every job's placement is cheap enough for each jobs tick, in place of lookups.
    /// The protocol lists them in one call. A replay does whatever its recording did.
    pub fn places_every_tick(&self) -> bool {
        match &self.source {
            Source::Wire(_) => true,
            Source::Cli(_) => false,
            Source::Replay(replay) => replay.holds(&Call::Placements),
        }
    }

    /// Asks one question and records it and its answer.
    ///
    /// # Errors
    ///
    /// Fails when the source fails, or the answer cannot be recorded.
    pub async fn ask(&mut self, call: Call) -> Result<Answer> {
        let at = self.now();
        let answer = match &mut self.source {
            Source::Wire(wire) => wire.answer(&call).await,
            Source::Cli(cli) => cli.answer(&call).await,
            Source::Replay(replay) => replay.answer(&call),
        };
        if let Some(recorder) = &mut self.recorder {
            recorder.write(&recording::Exchange {
                at,
                call,
                answer: answer
                    .as_ref()
                    .map(Clone::clone)
                    .map_err(|error| format!("{error:#}")),
            })?;
        }
        answer
    }

    /// Every job, including ones that ended within `MinJobAge`.
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn jobs(&mut self) -> Result<Vec<Attempt>> {
        match self.ask(Call::Jobs).await? {
            Answer::Jobs(jobs) => Ok(without_false_starts(jobs)),
            other => mismatched(&other),
        }
    }

    /// Every node, with its partitions merged.
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn nodes(&mut self) -> Result<Vec<Node>> {
        match self.ask(Call::Nodes).await? {
            Answer::Nodes(nodes) => Ok(nodes),
            other => mismatched(&other),
        }
    }

    /// Every partition, hidden ones included.
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn partitions(&mut self) -> Result<Vec<Partition>> {
        match self.ask(Call::Partitions).await? {
            Answer::Partitions(partitions) => Ok(partitions),
            other => mismatched(&other),
        }
    }

    /// The placement of every job the controller knows.
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn placements(&mut self) -> Result<Vec<Placed>> {
        match self.ask(Call::Placements).await? {
            Answer::Placements(placements) => Ok(placements),
            other => mismatched(&other),
        }
    }

    /// Looks up one job's placement. A job the controller has purged is [`Lookup::Gone`].
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn lookup(&mut self, attempt: &Attempt) -> Result<Lookup> {
        let call = Call::Lookup {
            key: attempt.key,
            printed: attempt.printed_id(),
        };
        match self.ask(call).await? {
            Answer::Lookup(lookup) => Ok(lookup),
            other => mismatched(&other),
        }
    }

    /// What accounting holds for `jobs`, every requeued attempt included.
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn outcomes(&mut self, jobs: &[JobId]) -> Result<Vec<Outcome>> {
        match self.ask(Call::Outcomes(jobs.to_vec())).await? {
            Answer::Outcomes(outcomes) => Ok(outcomes),
            other => mismatched(&other),
        }
    }

    /// Every allocation accounting holds that was pending or running between `from` and `to`,
    /// runaway records included.
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn history(&mut self, from: UnixTime, to: UnixTime) -> Result<Vec<Attempt>> {
        match self.ask(Call::History { from, to }).await? {
            Answer::History(attempts) => Ok(without_false_starts(attempts)),
            other => mismatched(&other),
        }
    }

    /// Scheduler and RPC counters.
    ///
    /// # Errors
    ///
    /// Fails when the source fails.
    pub async fn stats(&mut self) -> Result<Stats> {
        match self.ask(Call::Stats).await? {
            Answer::Stats(stats) => Ok(stats),
            other => mismatched(&other),
        }
    }

    /// Reads everything once: the cold start of a sampler.
    ///
    /// # Errors
    ///
    /// Fails when any call fails.
    pub async fn sample(&mut self) -> Result<Sample> {
        let at = self.now();
        let nodes = self.nodes().await?;
        let jobs = self.jobs().await?;
        let placements = self.placements().await?;
        Ok(Sample {
            at,
            site: self.site.clone(),
            partitions: self.partitions().await?,
            nodes,
            jobs,
            placements,
            stats: self.stats().await?,
        })
    }
}

fn mismatched<T>(answer: &Answer) -> Result<T> {
    bail!("an answer of the wrong kind: {answer:?}")
}

/// Everything read at once.
#[derive(Debug, Serialize)]
pub struct Sample {
    /// When the sample started.
    pub at: UnixTime,
    /// Site facts.
    pub site: Site,
    /// Partitions.
    pub partitions: Vec<Partition>,
    /// Nodes.
    pub nodes: Vec<Node>,
    /// Jobs.
    pub jobs: Vec<Attempt>,
    /// Placements.
    pub placements: Vec<Placed>,
    /// Scheduler counters.
    pub stats: Stats,
}

/// A node where running jobs' placed cores differ from the cores the node counts as allocated.
#[derive(Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// The node.
    pub node: NodeName,
    /// Cores that running jobs' placements hold, placed or not.
    pub placed: u32,
    /// The node's allocated CPUs, in cores.
    pub allocated: u32,
}

impl Sample {
    /// Compares each node's placed cores with its allocated cores. Jobs that start or end between
    /// the two commands show up here too.
    pub fn mismatches(&self) -> Vec<Mismatch> {
        let mut placed: HashMap<&NodeName, u32> = HashMap::new();
        let running = self
            .placements
            .iter()
            .filter(|job| job.state == JobState::Running);
        for cells in running.flat_map(|job| &job.placement.0) {
            let cores = u32::try_from(cells.cores.len()).unwrap_or(u32::MAX) + cells.unplaced;
            *placed.entry(&cells.node).or_default() += cores;
        }
        self.nodes
            .iter()
            .filter_map(|node| {
                let placed = placed.get(&node.name).copied().unwrap_or(0);
                let allocated = node.cpus_alloc / node.cpus_per_core();
                (placed != allocated).then(|| Mismatch {
                    node: node.name.clone(),
                    placed,
                    allocated,
                })
            })
            .collect()
    }
}

/// Clears the start of each ended attempt that held no nodes. slurmctld stamps a pending job it
/// cancels with a start equal to its end, though it never ran.
fn without_false_starts(mut attempts: Vec<Attempt>) -> Vec<Attempt> {
    for attempt in &mut attempts {
        if matches!(attempt.state, JobState::Ended(_)) && attempt.nodes.is_empty() {
            attempt.start = None;
        }
    }
    attempts
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn replay(fixture: &str) -> Slurm {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        Slurm::connect(Source::Replay(Replay::open(&path).unwrap()), None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn replayed_samples_place_every_allocated_core() {
        for (fixture, expected_running) in [
            ("slurm-23.11/quiet.jsonl", 5),
            ("slurm-23.11/busy.jsonl", 105),
        ] {
            let sample = replay(fixture).await.sample().await.unwrap();
            assert_eq!(sample.site.version, Version(23, 11, 4));
            assert_eq!(sample.nodes.len(), 51);
            let running = sample
                .jobs
                .iter()
                .filter(|job| job.state == JobState::Running)
                .count();
            assert_eq!(running, expected_running, "{fixture}");
            assert_eq!(sample.mismatches(), [], "{fixture}");
        }
    }

    #[tokio::test]
    async fn lookup_of_a_purged_job_is_gone() {
        let mut slurm = replay("slurm-23.11/quiet.jsonl").await;
        let sample = slurm.sample().await.unwrap();
        let mut purged = sample.jobs[0].clone();
        purged.key.job = JobId(1);
        purged.array = None;
        assert_eq!(slurm.lookup(&purged).await.unwrap(), Lookup::Gone);
    }

    #[tokio::test]
    async fn a_job_cancelled_while_pending_never_started() {
        use crate::slurm::recording::{Answer, read};
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/slurm-23.11/quiet.jsonl");
        let mut exchanges = read(&path).unwrap();
        let jobs = exchanges
            .iter_mut()
            .find_map(|exchange| match &mut exchange.answer {
                Ok(Answer::Jobs(jobs)) if exchange.call == Call::Jobs => Some(jobs),
                _ => None,
            })
            .unwrap();
        let mut cancelled = jobs[0].clone();
        cancelled.key.job = JobId(99_999_999);
        cancelled.state = JobState::Ended(glurmo_model::EndState::Cancelled);
        // slurmctld stamps a pending job it cancels with start = end = now.
        cancelled.start = Some(UnixTime(1_794_000_000));
        cancelled.end = cancelled.start;
        cancelled.nodes = Vec::new();
        jobs.push(cancelled.clone());
        let mut slurm = Slurm::connect(Source::Replay(Replay::new(exchanges)), None)
            .await
            .unwrap();
        let listed = slurm.jobs().await.unwrap();
        let read = listed.iter().find(|job| job.key == cancelled.key).unwrap();
        assert_eq!(read.start, None);
        assert_eq!(read.end, cancelled.end);
    }

    #[test]
    fn versions_order_and_print() {
        let version: Version = "23.11.4".parse().unwrap();
        assert_eq!(version, Version(23, 11, 4));
        assert!(version < Version(25, 11, 0));
        assert_eq!(
            "25.11.0-0rc1".parse::<Version>().unwrap(),
            Version(25, 11, 0)
        );
        assert_eq!(version.to_string(), "23.11.4");
        assert!("23.11".parse::<Version>().is_err());
    }
}
