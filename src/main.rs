//! `rtviz` shows who holds which cores and GPUs on a batch-scheduler cluster.

mod diff;
mod model;
mod rollup;
mod sampler;
mod slurm;
mod store;
mod xdg;

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::broadcast::error::RecvError;

use model::{Cluster, JobState, Owner};
use sampler::Sampler;
use slurm::source::{Exchange, Recorder, Replay, Source};
use slurm::{Intervals, Sample, Slurm};
use store::{Settings, Store};

/// Live view of who holds which cores and GPUs on a cluster.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print site facts, capabilities, each command's cost, and suggested intervals.
    Probe,
    /// Sample the cluster and print each tick's changes as a line of JSON.
    Sample {
        /// Read everything once, print it as JSON and exit.
        #[arg(long)]
        once: bool,
        /// Answer from a recording instead of the cluster.
        #[arg(long, value_name = "FILE")]
        replay: Option<PathBuf>,
        /// Keep history in this SQLite file, which must be on a local disk.
        #[arg(long, value_name = "FILE")]
        store: Option<PathBuf>,
        /// Days of accounting to backfill into a new store.
        #[arg(long, default_value_t = 30)]
        backfill_days: u32,
        /// Days of history to keep. Without it, history is kept forever.
        #[arg(long)]
        keep_days: Option<u32>,
    },
    /// Sample the cluster and save every command's output as JSON lines.
    Record {
        /// Rewrite logins, IDs, job names and paths, so the recording can become a test fixture.
        #[arg(long)]
        pseudonymize: bool,
        /// How long to keep sampling after the first tick.
        #[arg(long, default_value_t = 0)]
        minutes: i64,
        /// Where to write the recording.
        out: PathBuf,
    },
    /// Rewrite a raw recording's logins, IDs, job names and site addresses, so it can become a test fixture.
    Pseudonymize {
        /// The raw recording.
        raw: PathBuf,
        /// Where to write the rewritten recording.
        out: PathBuf,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Probe => {
            let mut slurm = Slurm::connect(Source::Live).await?;
            print!("{}", slurm::probe::probe(&mut slurm).await?);
        }
        Command::Sample {
            once,
            replay,
            store,
            backfill_days,
            keep_days,
        } => {
            let source = match replay {
                Some(path) => Source::Replay(Replay::open(&path)?),
                None => Source::Live,
            };
            let mut slurm = Slurm::connect(source).await?;
            if once {
                let sample = slurm.sample().await?;
                println!("{}", serde_json::to_string(&sample)?);
                summarize(&sample);
            } else {
                let store = match store {
                    Some(path) => Some(open_store(&path, backfill_days, keep_days)?),
                    None => None,
                };
                stream(slurm, store, None).await?;
            }
        }
        Command::Record {
            pseudonymize,
            minutes,
            out,
        } => {
            let slurm =
                Slurm::connect(Source::Record(Recorder::create(&out, pseudonymize)?)).await?;
            let until = model::UnixTime(slurm.now().0 + minutes * 60);
            stream(slurm, None, Some(until)).await?;
            eprintln!("recorded to {}", out.display());
        }
        Command::Pseudonymize { raw, out } => {
            let mut recorder = Recorder::create(&out, true)?;
            let file =
                File::open(&raw).with_context(|| format!("cannot open {}", raw.display()))?;
            for (number, line) in BufReader::new(file).lines().enumerate() {
                let exchange: Exchange = serde_json::from_str(&line?)
                    .with_context(|| format!("{} line {}", raw.display(), number + 1))?;
                recorder.write(&exchange)?;
            }
        }
    }
    Ok(())
}

/// Opens the store at `path`, with its nightly backup under `$XDG_DATA_HOME/rtviz`.
fn open_store(path: &std::path::Path, backfill_days: u32, keep_days: Option<u32>) -> Result<Store> {
    let name = path.file_name().context("the store path names no file")?;
    let settings = Settings {
        backfill_days,
        keep_days,
        backup: xdg::data_home()?.join("rtviz").join(name),
    };
    Store::open(path, settings)
}

/// Samples until `until`, printing each delta on stdout and a summary of the cluster after it on stderr.
async fn stream(slurm: Slurm, store: Option<Store>, until: Option<model::UnixTime>) -> Result<()> {
    let mut cluster = Cluster {
        hides_jobs: slurm.site().hides_jobs(),
        ..Cluster::default()
    };
    let mut sampler = Sampler::new(slurm, Intervals::default(), store);
    let mut deltas = sampler.subscribe();
    let sampling = async move { sampler.run(until).await };
    let printing = async {
        loop {
            match deltas.recv().await {
                Ok(delta) => {
                    cluster.apply(&delta);
                    println!("{}", serde_json::to_string(&*delta)?);
                    eprintln!(
                        "{} at {}: {} events, {} running, {} pending, {} nodes differ from sinfo",
                        delta.seq,
                        delta.at.0,
                        delta.events.len(),
                        cluster.running.len(),
                        cluster.queue.len(),
                        differing_nodes(&cluster)
                    );
                }
                Err(RecvError::Lagged(skipped)) => {
                    eprintln!("printing fell {skipped} deltas behind");
                }
                Err(RecvError::Closed) => return Ok::<_, anyhow::Error>(()),
            }
        }
    };
    tokio::join!(sampling, printing).1
}

/// Counts nodes where the cores that visible jobs hold differ from sinfo's allocated cores.
/// Jobs that start or end between squeue and sinfo show up here too.
fn differing_nodes(cluster: &Cluster) -> usize {
    let occupancy = cluster.occupancy();
    cluster
        .nodes
        .iter()
        .filter(|node| {
            let held = occupancy.get(&node.name).map_or(0, |cells| {
                cells
                    .cores
                    .iter()
                    .filter(|owner| matches!(owner, Some(Owner::Attempt(_))))
                    .count()
            });
            u32::try_from(held).ok() != Some(node.cpus_alloc / node.cpus_per_core())
        })
        .count()
}

/// Prints job counts and checks placed cores against sinfo on stderr.
fn summarize(sample: &Sample) {
    let count = |state: JobState| sample.jobs.iter().filter(|job| job.state == state).count();
    let pending_tasks: u32 = sample
        .jobs
        .iter()
        .filter(|job| job.state == JobState::Pending)
        .map(|job| job.array.as_ref().map_or(1, |array| array.count))
        .sum();
    eprintln!(
        "{} running, {} pending ({pending_tasks} tasks), {} completing, {} nodes",
        count(JobState::Running),
        count(JobState::Pending),
        count(JobState::Completing),
        sample.nodes.len()
    );
    let inexact = sample
        .placements
        .iter()
        .filter(|job| !job.placement.is_exact())
        .count();
    if inexact > 0 {
        eprintln!("{inexact} placements have cut-off CPU_IDs");
    }
    let mismatches = sample.mismatches();
    eprintln!(
        "placed cores match sinfo on {} of {} nodes",
        sample.nodes.len() - mismatches.len(),
        sample.nodes.len()
    );
    for mismatch in mismatches {
        eprintln!(
            "  {}: {} placed, {} allocated",
            mismatch.node.0, mismatch.placed, mismatch.allocated
        );
    }
}
