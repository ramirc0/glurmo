//! The only code that knows Slurm: commands, output formats and quirks.

pub mod fields;
pub mod hostlist;
pub mod probe;
pub mod scontrol;
pub mod sdiag;
pub mod sinfo;
pub mod source;
pub mod squeue;

use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::{Serialize, Serializer};

use crate::model::{Attempt, JobState, Node, NodeName, Partition, UnixTime};
use scontrol::{CpuMap, Placed};
use sdiag::Stats;
use source::{Exchange, Source};

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

/// The controller's per-user RPC rate limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RateLimit {
    /// Tokens a user can save up.
    pub bucket_size: u32,
    /// Tokens added each period.
    pub refill_rate: u32,
    /// The refill period.
    pub refill_period_s: u32,
}

/// Facts about the site, read from the controller's configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Site {
    /// The cluster name.
    pub cluster: String,
    /// The controller's Slurm version.
    pub version: Version,
    /// How long ended jobs stay in squeue.
    pub min_job_age_s: u64,
    /// What the controller hides from other users, such as `jobs` or `nodes`.
    pub private_data: Vec<String>,
    /// The RPC rate limit, if the controller enforces one.
    pub rate_limit: Option<RateLimit>,
}

/// The answer to a placement lookup.
#[derive(Debug, PartialEq)]
pub enum Lookup {
    /// The job and its placement.
    Found(Placed),
    /// The controller no longer knows the job.
    Gone,
}

/// Runs Slurm commands through a [`Source`] and parses their output into model types.
pub struct Slurm {
    source: Source,
    site: Site,
}

impl Slurm {
    /// Reads the site's configuration through `source`.
    ///
    /// # Errors
    ///
    /// Fails when `scontrol show config` fails or lacks a needed setting.
    pub async fn connect(mut source: Source) -> Result<Self> {
        let exchange = source.run(scontrol::config_argv()).await?;
        let site = checked(&exchange, scontrol::parse_config)?;
        Ok(Self { source, site })
    }

    /// The site facts read at connection.
    pub fn site(&self) -> &Site {
        &self.site
    }

    /// The current time, which a replay drives.
    pub fn now(&self) -> UnixTime {
        self.source.now()
    }

    /// Every job squeue lists, including ones that ended within `MinJobAge`.
    ///
    /// # Errors
    ///
    /// Fails when squeue fails, writes to stderr, or prints output that does not parse.
    pub async fn jobs(&mut self) -> Result<Vec<Attempt>> {
        self.query(squeue::argv(), squeue::parse).await
    }

    /// Every node, with its partitions merged.
    ///
    /// # Errors
    ///
    /// Fails when sinfo fails, writes to stderr, or prints output that does not parse.
    pub async fn nodes(&mut self) -> Result<Vec<Node>> {
        self.query(sinfo::argv(), sinfo::parse).await
    }

    /// Every partition, hidden ones included.
    ///
    /// # Errors
    ///
    /// Fails when scontrol fails, writes to stderr, or prints output that does not parse.
    pub async fn partitions(&mut self) -> Result<Vec<Partition>> {
        self.query(scontrol::partitions_argv(), scontrol::parse_partitions)
            .await
    }

    /// The placement of every job the controller knows, for a cold start.
    ///
    /// # Errors
    ///
    /// Fails when scontrol fails, writes to stderr, prints output that does not parse,
    /// or names a node missing from `nodes`.
    pub async fn placements(&mut self, nodes: &[Node]) -> Result<Vec<Placed>> {
        let map = CpuMap::new(nodes, self.site.version);
        self.query(scontrol::jobs_argv(), |stdout| {
            scontrol::parse_jobs(stdout, &map)
        })
        .await
    }

    /// Looks up one job's placement. A job the controller has purged is [`Lookup::Gone`].
    ///
    /// # Errors
    ///
    /// Fails like [`Slurm::placements`], or when scontrol prints no job.
    pub async fn lookup(&mut self, attempt: &Attempt, nodes: &[Node]) -> Result<Lookup> {
        let map = CpuMap::new(nodes, self.site.version);
        let exchange = self.source.run(scontrol::lookup_argv(attempt)).await?;
        if exchange.status != 0 && exchange.stderr.contains(scontrol::GONE) {
            return Ok(Lookup::Gone);
        }
        let mut jobs = checked(&exchange, |stdout| scontrol::parse_jobs(stdout, &map))?;
        if jobs.is_empty() {
            bail!("scontrol printed no job for {}", exchange.argv.join(" "));
        }
        Ok(Lookup::Found(jobs.swap_remove(0)))
    }

    /// Scheduler and RPC counters.
    ///
    /// # Errors
    ///
    /// Fails when sdiag fails, writes to stderr, or prints output that does not parse.
    pub async fn stats(&mut self) -> Result<Stats> {
        self.query(sdiag::argv(), sdiag::parse).await
    }

    /// Runs any command through the source, without parsing.
    ///
    /// # Errors
    ///
    /// Fails like [`Source::run`].
    pub async fn run(&mut self, argv: Vec<String>) -> Result<Exchange> {
        self.source.run(argv).await
    }

    /// Reads everything once: the cold start of a sampler.
    ///
    /// # Errors
    ///
    /// Fails when any command fails.
    pub async fn sample(&mut self) -> Result<Sample> {
        let at = self.now();
        let nodes = self.nodes().await?;
        let jobs = self.jobs().await?;
        let placements = self.placements(&nodes).await?;
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

    async fn query<T>(
        &mut self,
        argv: Vec<String>,
        parse: impl FnOnce(&str) -> Result<T>,
    ) -> Result<T> {
        let exchange = self.source.run(argv).await?;
        checked(&exchange, parse)
    }
}

/// Parses a command's output once it has exited cleanly and written nothing to stderr.
/// Output that fails to parse is kept for a test fixture.
fn checked<T>(exchange: &Exchange, parse: impl FnOnce(&str) -> Result<T>) -> Result<T> {
    let command = exchange.argv.join(" ");
    if exchange.status != 0 || !exchange.stderr.is_empty() {
        bail!(
            "{command} exited with {}: {}",
            exchange.status,
            exchange.stderr.trim()
        );
    }
    parse(&exchange.stdout).map_err(|error| match keep_unparsed(exchange) {
        Ok(path) => error.context(format!("{command}: output saved to {}", path.display())),
        Err(save) => error.context(format!("{command}: output not saved: {save:#}")),
    })
}

/// Appends an exchange to `$XDG_STATE_HOME/rtviz/unparsed.jsonl`.
fn keep_unparsed(exchange: &Exchange) -> Result<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("neither XDG_STATE_HOME nor HOME is set")?
        .join("rtviz");
    fs::create_dir_all(&state)?;
    let path = state.join("unparsed.jsonl");
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{}", serde_json::to_string(exchange)?)?;
    Ok(path)
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
    /// Jobs as squeue lists them.
    pub jobs: Vec<Attempt>,
    /// Placements as scontrol reports them.
    pub placements: Vec<Placed>,
    /// Scheduler counters.
    pub stats: Stats,
}

/// A node where running jobs' placed cores differ from the cores sinfo counts as allocated.
#[derive(Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// The node.
    pub node: NodeName,
    /// Cores that running jobs' placements hold, placed or not.
    pub placed: u32,
    /// sinfo's allocated CPUs, in cores.
    pub allocated: u32,
}

impl Sample {
    /// Compares each node's placed cores with sinfo's allocated cores. Jobs that start or end between
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

#[cfg(test)]
mod tests {
    use super::*;

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
