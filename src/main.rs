//! `rtviz` shows who holds which cores and GPUs on a batch-scheduler cluster.

mod model;
mod slurm;

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use model::JobState;
use slurm::source::{Replay, Source};
use slurm::{Sample, Slurm};

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
    /// Read the cluster and print it as JSON.
    Sample {
        /// Read once and exit.
        #[arg(long)]
        once: bool,
        /// Answer from a recording instead of the cluster.
        #[arg(long, value_name = "FILE")]
        replay: Option<PathBuf>,
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
            if !once {
                bail!("only `sample --once` exists so far");
            }
            let source = match replay {
                Some(path) => Source::Replay(Replay::open(&path)?),
                None => Source::Live,
            };
            let sample = Slurm::connect(source).await?.sample().await?;
            println!("{}", serde_json::to_string(&sample)?);
            summarize(&sample);
        }
    }
    Ok(())
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
