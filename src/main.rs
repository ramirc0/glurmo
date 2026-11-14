//! `rtviz` shows who holds which cores and GPUs on a batch-scheduler cluster.

mod diff;
mod model;
mod sampler;
mod slurm;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::broadcast::error::RecvError;

use model::{Cluster, JobState, Owner};
use sampler::Sampler;
use slurm::source::{Recorder, Replay, Source};
use slurm::{Intervals, Sample, Slurm};

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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Probe => {
            let mut slurm = Slurm::connect(Source::Live).await?;
            print!("{}", slurm::probe::probe(&mut slurm).await?);
        }
        Command::Sample { once, replay } => {
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
                stream(slurm, None).await?;
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
            stream(slurm, Some(until)).await?;
            eprintln!("recorded to {}", out.display());
        }
    }
    Ok(())
}

/// Samples until `until`, printing each delta on stdout and a summary of the cluster after it on stderr.
async fn stream(slurm: Slurm, until: Option<model::UnixTime>) -> Result<()> {
    let mut cluster = Cluster {
        hides_jobs: slurm.site().hides_jobs(),
        ..Cluster::default()
    };
    let mut sampler = Sampler::new(slurm, Intervals::default());
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
