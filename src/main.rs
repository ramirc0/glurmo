//! `glurmo` shows who holds which cores and GPUs on a batch-scheduler cluster.

mod config;
mod diff;
#[cfg(test)]
mod fixture;
mod hub;
mod library;
mod passwd;
mod past;
mod rollup;
mod sampler;
mod server;
mod slurm;
mod store;
mod tui;
mod xdg;

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use config::{Config, Keep, Storage};
use glurmo_model::{Cluster, JobState, Owner};
use hub::Hub;
use library::Library;
use sampler::Sampler;
use slurm::Source;
use slurm::recording::{Recorder, Replay};
use slurm::{Sample, Slurm};
use store::{History, Settings, Store};

/// Live view of who holds which cores and GPUs on a cluster.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// The config file. Without it, `$XDG_CONFIG_HOME/glurmo/config.ron` is read if it exists.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sample the cluster, keep its history and serve both over HTTP.
    Serve {
        /// Answer from a recording at the pace it was recorded, with history in a throwaway store, and
        /// keep serving its last state once it runs out.
        #[arg(long, value_name = "FILE")]
        replay: Option<PathBuf>,
        /// Play the recording this many times faster than it was recorded.
        #[arg(long, default_value_t = 1, requires = "replay")]
        speed: u32,
    },
    /// Show the cluster in the terminal, following a server or sampling in-process.
    Tui {
        /// The server to follow, such as `http://head:8765`. It overrides the config's.
        #[arg(long, value_name = "URL", conflicts_with = "embedded")]
        server: Option<String>,
        /// Sample the cluster in-process, even when the config names a server.
        #[arg(long)]
        embedded: bool,
    },
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
        /// Keep history in the store the config names.
        #[arg(long)]
        store: bool,
    },
    /// Sample the cluster and save every call and its answer as JSON lines.
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
    /// Rewrite a raw recording's logins and job names, so it can become a test fixture.
    Pseudonymize {
        /// The raw recording.
        raw: PathBuf,
        /// Where to write the rewritten recording.
        out: PathBuf,
    },
    /// Turn a recording of Slurm's command output, which glurmo made before it spoke the protocol, into
    /// one of calls and answers.
    Convert {
        /// The recording of command output.
        old: PathBuf,
        /// Where to write the converted recording.
        out: PathBuf,
    },
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    match cli.command {
        Command::Serve { replay, speed } => serve(&config, replay.as_deref(), speed).await?,
        Command::Tui { server, embedded } => {
            let server = if embedded {
                None
            } else {
                server.or_else(|| config.tui.server.clone())
            };
            tui::run(&config, server.as_deref()).await?;
        }
        Command::Probe => {
            let mut slurm = Slurm::connect(Source::live(&config.slurm)?, None).await?;
            print!("{}", slurm::probe::probe(&mut slurm).await?);
        }
        Command::Sample {
            once,
            replay,
            store,
        } => {
            let source = match replay {
                Some(path) => Source::Replay(Replay::open(&path)?),
                None => Source::live(&config.slurm)?,
            };
            let mut slurm = Slurm::connect(source, None).await?;
            if once {
                let sample = slurm.sample().await?;
                println!("{}", serde_json::to_string(&sample)?);
                summarize(&sample);
            } else {
                let store = if store {
                    let path = store_path(&config.store, &slurm.site().cluster)?;
                    Some(open_store(&config.store, &path)?)
                } else {
                    None
                };
                stream(slurm, &config, store, None).await?;
            }
        }
        Command::Record {
            pseudonymize,
            minutes,
            out,
        } => {
            let recorder = Recorder::create(&out, pseudonymize)?;
            let slurm = Slurm::connect(Source::live(&config.slurm)?, Some(recorder)).await?;
            let until = glurmo_model::UnixTime(slurm.now().0 + minutes * 60);
            stream(slurm, &config, None, Some(until)).await?;
            eprintln!("recorded to {}", out.display());
        }
        Command::Pseudonymize { raw, out } => {
            let mut recorder = Recorder::create(&out, true)?;
            for exchange in slurm::recording::read(&raw)? {
                recorder.write(&exchange)?;
            }
        }
        Command::Convert { old, out } => {
            let file =
                File::open(&old).with_context(|| format!("cannot open {}", old.display()))?;
            let exchanges = BufReader::new(file)
                .lines()
                .enumerate()
                .map(|(number, line)| {
                    serde_json::from_str(&line?)
                        .with_context(|| format!("{} line {}", old.display(), number + 1))
                })
                .collect::<Result<Vec<slurm::cli::Exchange>>>()?;
            let mut recorder = Recorder::create(&out, false)?;
            for exchange in slurm::cli::convert(&exchanges)? {
                recorder.write(&exchange)?;
            }
        }
    }
    Ok(())
}

/// Samples into the store and serves the stream, history, themes and layouts until the process ends.
/// Theme and layout files are read from `$XDG_CONFIG_HOME/glurmo`. With `replay`, the samples come from a
/// recording, played `speed` times faster than it was recorded, and the store is a temporary one.
async fn serve(config: &Config, replay: Option<&Path>, speed: u32) -> Result<()> {
    let scratch = tempfile::tempdir()?;
    let (slurm, path, store) = if let Some(recording) = replay {
        let slurm =
            Slurm::connect(Source::Replay(Replay::open(recording)?.paced(speed)), None).await?;
        let path = scratch.path().join("replay.sqlite");
        let settings = Settings {
            backfill_days: 0,
            keep_days: None,
            backup: scratch.path().join("backup.sqlite"),
        };
        let store = Store::open(&path, settings)?;
        (slurm, path, store)
    } else {
        let slurm = Slurm::connect(Source::live(&config.slurm)?, None).await?;
        let path = store_path(&config.store, &slurm.site().cluster)?;
        let store = open_store(&config.store, &path)?;
        (slurm, path, store)
    };
    let hub = Hub::new(slurm.site().clone());
    let dir = xdg::config_home()?.join("glurmo");
    let (library, _) = watch::channel(Library::load(&dir));
    let router = server::router(hub.clone(), History::open(&path), library.subscribe());
    let mut sampler = Sampler::new(slurm, config, Some(store), hub);
    eprintln!(
        "serving on http://{} with history in {}",
        config.server.listen,
        path.display()
    );
    let sample = async {
        sampler.run(None).await;
        if replay.is_some() {
            eprintln!("the replay ran out; serving its last state");
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        () = sample => Ok(()),
        () = library::keep_current(&dir, &library) => Ok(()),
        served = server::serve(config.server.listen, router) => served,
    }
}

/// Where the config puts the store, by default `$XDG_STATE_HOME/glurmo/<cluster>.sqlite`.
fn store_path(storage: &Storage, cluster: &str) -> Result<PathBuf> {
    Ok(match &storage.path {
        Some(path) => path.clone(),
        None => xdg::state_home()?
            .join("glurmo")
            .join(format!("{cluster}.sqlite")),
    })
}

/// Opens the store at `path`. Its nightly backup goes to `$XDG_DATA_HOME/glurmo` under the same file name.
fn open_store(storage: &Storage, path: &Path) -> Result<Store> {
    let name = path.file_name().context("the store path names no file")?;
    let settings = Settings {
        backfill_days: storage.backfill.0,
        keep_days: match storage.keep {
            Keep::Forever => None,
            Keep::Days(days) => Some(days),
        },
        backup: xdg::data_home()?.join("glurmo").join(name),
    };
    Store::open(path, settings)
}

/// Samples until `until`, printing each delta on stdout and a summary of the cluster after it on stderr.
async fn stream(
    slurm: Slurm,
    config: &Config,
    store: Option<Store>,
    until: Option<glurmo_model::UnixTime>,
) -> Result<()> {
    let mut cluster = Cluster {
        hides_jobs: slurm.site().hides_jobs(),
        ..Cluster::default()
    };
    let hub = Hub::new(slurm.site().clone());
    let mut sampler = Sampler::new(slurm, config, store, hub.clone());
    let mut deltas = hub.subscribe();
    let sampling = async move { sampler.run(until).await };
    let printing = async {
        loop {
            match deltas.recv().await {
                Ok(delta) => {
                    cluster.apply(&delta);
                    println!("{}", serde_json::to_string(&*delta)?);
                    eprintln!(
                        "{} at {}: {} events, {} running, {} pending, {} nodes differ from their allocation",
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

/// Counts nodes where the cores that visible jobs hold differ from the node's allocated cores.
/// Jobs that start or end between the two calls show up here too.
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

/// Prints job counts and checks placed cores against each node's allocation on stderr.
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
        "placed cores match the allocation on {} of {} nodes",
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
