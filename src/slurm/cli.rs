//! The fallback backend: Slurm's commands, run on this host and parsed. Also turns recordings of their
//! output, which glurmo made before it spoke the protocol, into typed recordings.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::mem::Discriminant;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::recording::{Answer, Call};
use super::scontrol::CpuMap;
use super::{Lookup, Version, sacct, scontrol, sdiag, sinfo, squeue};
use glurmo_model::{AttemptKey, JobId, Node, UnixTime};

/// One command and its output. Recordings from before the protocol hold one per line, as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exchange {
    /// When the command started.
    pub at: UnixTime,
    /// The command and its arguments.
    pub argv: Vec<String>,
    /// The exit status, or -1 when a signal killed it.
    pub status: i32,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
}

const TIMEOUT: Duration = Duration::from_secs(15);

/// Answers calls by running Slurm's commands. Placement needs each node's threads per core, so it
/// keeps the nodes from its last sinfo.
#[derive(Default)]
pub struct Cli {
    version: Option<Version>,
    nodes: Vec<Node>,
    /// sacct lines already reported as unreadable, so a job asked about every tick is saved once.
    reported: HashSet<String>,
    /// Kinds of call whose unparsed output this run has saved, so a listing that fails every tick is
    /// saved once.
    saved: HashSet<Discriminant<Call>>,
}

impl Cli {
    /// Runs the command for `call` and parses its output. Output that fails to parse is kept for a
    /// test fixture, once per kind of call in a run.
    ///
    /// # Errors
    ///
    /// Fails when the command cannot run, fails, writes to stderr, or prints output that does not parse.
    pub async fn answer(&mut self, call: &Call) -> Result<Answer> {
        if matches!(call, Call::Placements | Call::Lookup { .. }) && self.nodes.is_empty() {
            self.parse(&Call::Nodes, &run(sinfo::argv(), TIMEOUT).await?)?;
        }
        let exchange = run(argv(call)?, TIMEOUT).await?;
        self.parse(call, &exchange).map_err(|error| {
            if exchange.status != 0 {
                return error;
            }
            match unparsed_dir().and_then(|dir| self.keep_first_unparsed(call, &exchange, &dir)) {
                Ok(Some(path)) => error.context(format!("output saved to {}", path.display())),
                Ok(None) => error,
                Err(save) => error.context(format!("output not saved: {save:#}")),
            }
        })
    }

    /// Saves the output of a call that did not parse, if no call of its kind has been saved this run.
    fn keep_first_unparsed(
        &mut self,
        call: &Call,
        exchange: &Exchange,
        dir: &Path,
    ) -> Result<Option<PathBuf>> {
        if !self.saved.insert(std::mem::discriminant(call)) {
            return Ok(None);
        }
        keep_unparsed(exchange, dir).map(Some)
    }

    fn parse(&mut self, call: &Call, exchange: &Exchange) -> Result<Answer> {
        let version = self.version.unwrap_or(Version(23, 11, 4));
        if let Call::Lookup { .. } = call
            && exchange.status != 0
            && (exchange.stderr.contains(scontrol::GONE)
                || scontrol::is_not_found(&exchange.stdout))
        {
            return Ok(Answer::Lookup(Lookup::Gone));
        }
        let command = exchange.argv.join(" ");
        if exchange.status != 0 || !exchange.stderr.is_empty() {
            bail!(
                "{command} exited with {}: {}",
                exchange.status,
                exchange.stderr.trim()
            );
        }
        let stdout = exchange.stdout.as_str();
        let answer = match call {
            Call::Config => {
                let site = scontrol::parse_config(stdout)?;
                self.version = Some(site.version);
                Answer::Config(site)
            }
            Call::Jobs => Answer::Jobs(squeue::parse(stdout)?),
            Call::Nodes => {
                self.nodes = sinfo::parse(stdout)?;
                Answer::Nodes(self.nodes.clone())
            }
            Call::Partitions => Answer::Partitions(scontrol::parse_partitions(stdout)?),
            Call::Placements => Answer::Placements(scontrol::parse_jobs(
                stdout,
                &CpuMap::new(&self.nodes, version),
            )?),
            Call::Lookup { .. } => {
                let mut jobs = scontrol::parse_jobs(stdout, &CpuMap::new(&self.nodes, version))?;
                if jobs.is_empty() {
                    bail!("scontrol printed no job for {command}");
                }
                Answer::Lookup(Lookup::Found(jobs.swap_remove(0)))
            }
            Call::Outcomes(_) => Answer::Outcomes(self.readable(exchange, sacct::parse(stdout))),
            Call::History { .. } => {
                Answer::History(self.readable(exchange, sacct::parse_history(stdout)))
            }
            Call::Stats => Answer::Stats(sdiag::parse(stdout)?),
        };
        Ok(answer)
    }
}

impl Cli {
    /// The records of a sacct read. Lines that did not parse are logged and kept for a fixture, once
    /// each, so one cannot block the rest.
    fn readable<T>(&mut self, exchange: &Exchange, read: sacct::Read<T>) -> Vec<T> {
        let new: Vec<_> = read
            .unreadable
            .into_iter()
            .filter(|unreadable| self.reported.insert(unreadable.line.clone()))
            .collect();
        if !new.is_empty() {
            let kept = unparsed_dir()
                .and_then(|dir| keep_unparsed(exchange, &dir))
                .map_or_else(
                    |error| format!("not saved: {error:#}"),
                    |path| format!("saved to {}", path.display()),
                );
            for unreadable in &new {
                eprintln!(
                    "skipped a sacct line ({kept}): {}: {:?}",
                    unreadable.error, unreadable.line
                );
            }
        }
        read.records
    }
}

/// The command that answers `call`.
fn argv(call: &Call) -> Result<Vec<String>> {
    Ok(match call {
        Call::Config => scontrol::config_argv(),
        Call::Jobs => squeue::argv(),
        Call::Nodes => sinfo::argv(),
        Call::Partitions => scontrol::partitions_argv(),
        Call::Placements => scontrol::jobs_argv(),
        Call::Lookup { printed, .. } => {
            let mut argv = scontrol::jobs_argv();
            argv.push(printed.clone());
            argv
        }
        Call::Outcomes(jobs) => sacct::argv(jobs),
        Call::History { from, to } => sacct::history_argv(*from, *to)?,
        Call::Stats => sdiag::argv(),
    })
}

/// Runs a command in an environment that cannot change Slurm's output format.
async fn run(argv: Vec<String>, timeout: Duration) -> Result<Exchange> {
    let (program, rest) = argv.split_first().context("empty command")?;
    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env("SLURM_TIME_FORMAT", "%s")
        .env("SLURM_BITSTR_LEN", "0")
        .env("TZ", "UTC");
    for (key, _) in std::env::vars_os() {
        if is_scrubbed(&key.to_string_lossy()) {
            command.env_remove(key);
        }
    }
    let at = UnixTime::now();
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .with_context(|| format!("{program} ran past {timeout:?}"))?
        .with_context(|| format!("cannot run {program}"))?;
    Ok(Exchange {
        at,
        argv,
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Whether a variable could filter or reformat Slurm's output.
fn is_scrubbed(key: &str) -> bool {
    ["SQUEUE_", "SINFO_", "SACCT_"]
        .iter()
        .any(|prefix| key.starts_with(prefix))
        || key == "SLURM_CLUSTERS"
}

/// Where output that did not parse goes: `$XDG_STATE_HOME/glurmo`.
fn unparsed_dir() -> Result<PathBuf> {
    Ok(crate::xdg::state_home()?.join("glurmo"))
}

/// Appends an exchange to `unparsed.jsonl` in `dir`.
fn keep_unparsed(exchange: &Exchange, dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join("unparsed.jsonl");
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{}", serde_json::to_string(exchange)?)?;
    Ok(path)
}

/// Turns a recording of command output into a typed one. A lookup names its attempt by the ID squeue
/// printed, so its key comes from the last squeue before it.
///
/// # Errors
///
/// Fails on a command glurmo never runs, or a lookup of an attempt no squeue listed.
pub fn convert(exchanges: &[Exchange]) -> Result<Vec<super::recording::Exchange>> {
    let mut cli = Cli::default();
    let mut keys: HashMap<String, AttemptKey> = HashMap::new();
    let mut typed = Vec::new();
    for exchange in exchanges {
        let call = call(&exchange.argv, &keys)
            .with_context(|| format!("at {}: {}", exchange.at.0, exchange.argv.join(" ")))?;
        let answer = cli
            .parse(&call, exchange)
            .map_err(|error| format!("{error:#}"));
        if let Ok(Answer::Jobs(attempts)) = &answer {
            keys = attempts
                .iter()
                .map(|attempt| (attempt.printed_id(), attempt.key))
                .collect();
        }
        typed.push(super::recording::Exchange {
            at: exchange.at,
            call,
            answer,
        });
    }
    Ok(typed)
}

/// The call a recorded command answered.
fn call(argv: &[String], keys: &HashMap<String, AttemptKey>) -> Result<Call> {
    let jobs = scontrol::jobs_argv();
    Ok(if argv == scontrol::config_argv() {
        Call::Config
    } else if argv == squeue::argv() {
        Call::Jobs
    } else if argv == sinfo::argv() {
        Call::Nodes
    } else if argv == scontrol::partitions_argv() {
        Call::Partitions
    } else if argv == jobs {
        Call::Placements
    } else if argv.len() == jobs.len() + 1 && argv.starts_with(&jobs) {
        let printed = argv[jobs.len()].clone();
        let key = *keys
            .get(&printed)
            .with_context(|| format!("no squeue listed {printed}"))?;
        Call::Lookup { key, printed }
    } else if argv == sdiag::argv() {
        Call::Stats
    } else if argv.first().is_some_and(|program| program == "sacct") {
        let after = |flag: &str| {
            argv.iter()
                .position(|word| word == flag)
                .and_then(|i| argv.get(i + 1))
                .with_context(|| format!("sacct without {flag}"))
        };
        if argv.iter().any(|word| word == "-S") {
            let time = |flag| -> Result<UnixTime> {
                let civil: jiff::civil::DateTime = after(flag)?.parse()?;
                Ok(UnixTime(
                    civil
                        .to_zoned(jiff::tz::TimeZone::UTC)?
                        .timestamp()
                        .as_second(),
                ))
            };
            Call::History {
                from: time("-S")?,
                to: time("-E")?,
            }
        } else {
            let ids = after("-j")?
                .split(',')
                .map(|id| Ok(JobId(id.parse()?)))
                .collect::<Result<_>>()?;
            Call::Outcomes(ids)
        }
    } else {
        bail!("glurmo never runs this command")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn scrubs_variables_that_change_output() {
        for key in [
            "SQUEUE_FORMAT",
            "SINFO_SORT",
            "SACCT_FORMAT",
            "SLURM_CLUSTERS",
        ] {
            assert!(is_scrubbed(key), "{key}");
        }
        for key in ["SLURM_CONF", "PATH", "SQUEUE"] {
            assert!(!is_scrubbed(key), "{key}");
        }
    }

    #[tokio::test]
    async fn run_sets_epoch_times_full_bitstrings_and_utc() {
        let exchange = run(argv(&["env"]), TIMEOUT).await.unwrap();
        assert_eq!(exchange.status, 0);
        for line in ["SLURM_TIME_FORMAT=%s", "SLURM_BITSTR_LEN=0", "TZ=UTC"] {
            assert!(exchange.stdout.lines().any(|out| out == line), "{line}");
        }
    }

    #[tokio::test]
    async fn run_keeps_status_and_stderr() {
        let exchange = run(argv(&["sh", "-c", "echo oops >&2; exit 3"]), TIMEOUT)
            .await
            .unwrap();
        assert_eq!((exchange.status, exchange.stderr.as_str()), (3, "oops\n"));
    }

    #[tokio::test]
    async fn run_kills_commands_past_the_timeout() {
        let started = std::time::Instant::now();
        let result = run(argv(&["sleep", "5"]), Duration::from_millis(100)).await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_lookup_of_an_array_task_no_longer_listed_is_gone() {
        let call = Call::Lookup {
            key: AttemptKey {
                job: JobId(3_848_384),
                submit: UnixTime(1_794_000_000),
            },
            printed: "3848210_174".into(),
        };
        let exchange = Exchange {
            at: UnixTime(1_794_000_100),
            argv: argv(&["scontrol", "-d", "-o", "show", "job", "3848210_174"]),
            status: 1,
            stdout: "Job 3848210_174 not found\n".into(),
            stderr: String::new(),
        };
        assert_eq!(
            Cli::default().parse(&call, &exchange).unwrap(),
            Answer::Lookup(Lookup::Gone)
        );
    }

    #[test]
    fn unparsed_output_is_saved_once_per_kind_of_call() {
        let dir = tempfile::tempdir().unwrap();
        let exchange = Exchange {
            at: UnixTime(1_794_000_000),
            argv: argv(&["squeue"]),
            status: 0,
            stdout: "garbage\n".into(),
            stderr: String::new(),
        };
        let mut cli = Cli::default();
        let lines = || {
            fs::read_to_string(dir.path().join("unparsed.jsonl"))
                .unwrap()
                .lines()
                .count()
        };
        for _ in 0..2 {
            cli.keep_first_unparsed(&Call::Jobs, &exchange, dir.path())
                .unwrap();
        }
        assert_eq!(lines(), 1);
        cli.keep_first_unparsed(&Call::Nodes, &exchange, dir.path())
            .unwrap();
        assert_eq!(lines(), 2);
    }

    #[test]
    fn sacct_windows_convert_back_to_epoch_seconds() {
        let from = UnixTime(1_794_695_400);
        let to = UnixTime(1_794_699_000);
        let history = sacct::history_argv(from, to).unwrap();
        assert_eq!(
            call(&history, &HashMap::new()).unwrap(),
            Call::History { from, to }
        );
        let outcomes = sacct::argv(&[JobId(5), JobId(9)]);
        assert_eq!(
            call(&outcomes, &HashMap::new()).unwrap(),
            Call::Outcomes(vec![JobId(5), JobId(9)])
        );
    }
}
