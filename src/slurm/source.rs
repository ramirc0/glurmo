//! Where command output comes from: the live cluster or a recording.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::scontrol;
use crate::model::UnixTime;

/// One command and its answer. A recording holds one per line, as JSON.
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

/// Where command output comes from.
pub enum Source {
    /// Run commands on this host.
    Live,
    /// Answer from a recording.
    Replay(Replay),
}

impl Source {
    /// Runs a command, or answers it from the recording.
    ///
    /// # Errors
    ///
    /// Fails when the command cannot start or runs past the timeout,
    /// or when the recording holds no answer for it.
    pub async fn run(&mut self, argv: Vec<String>) -> Result<Exchange> {
        match self {
            Self::Live => live(argv, TIMEOUT).await,
            Self::Replay(replay) => replay.answer(argv),
        }
    }

    /// The current time. A replay's clock reads the time of the last answer.
    pub fn now(&self) -> UnixTime {
        match self {
            Self::Live => UnixTime::now(),
            Self::Replay(replay) => replay.now,
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(15);

/// Runs a command in an environment that cannot change Slurm's output format.
async fn live(argv: Vec<String>, timeout: Duration) -> Result<Exchange> {
    let (program, rest) = argv.split_first().context("empty command")?;
    let mut command = Command::new(program);
    command
        .args(rest)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env("SLURM_TIME_FORMAT", "%s")
        .env("SLURM_BITSTR_LEN", "0");
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

/// Answers commands from a recording.
///
/// Each command gets its recorded answers in order. A lookup with no answer left gets
/// "Invalid job id specified", as a job that has left the controller would.
pub struct Replay {
    answers: HashMap<Vec<String>, VecDeque<Exchange>>,
    now: UnixTime,
}

impl Replay {
    /// Reads a recording.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or a line is not an exchange.
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mut replay = Self {
            answers: HashMap::new(),
            now: UnixTime(0),
        };
        for (number, line) in BufReader::new(file).lines().enumerate() {
            let exchange: Exchange = serde_json::from_str(&line?)
                .with_context(|| format!("{} line {}", path.display(), number + 1))?;
            if number == 0 {
                replay.now = exchange.at;
            }
            replay
                .answers
                .entry(exchange.argv.clone())
                .or_default()
                .push_back(exchange);
        }
        Ok(replay)
    }

    fn answer(&mut self, argv: Vec<String>) -> Result<Exchange> {
        if let Some(exchange) = self.answers.get_mut(&argv).and_then(VecDeque::pop_front) {
            self.now = self.now.max(exchange.at);
            return Ok(exchange);
        }
        let jobs = scontrol::jobs_argv();
        if argv.len() == jobs.len() + 1 && argv.starts_with(&jobs) {
            return Ok(Exchange {
                at: self.now,
                argv,
                status: 1,
                stdout: String::new(),
                stderr: format!("slurm_load_jobs error: {}\n", scontrol::GONE),
            });
        }
        bail!("the recording has no answer left for {}", argv.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
    async fn live_sets_epoch_times_and_full_bitstrings() {
        let exchange = live(argv(&["env"]), TIMEOUT).await.unwrap();
        assert_eq!(exchange.status, 0);
        assert!(
            exchange
                .stdout
                .lines()
                .any(|line| line == "SLURM_TIME_FORMAT=%s")
        );
        assert!(
            exchange
                .stdout
                .lines()
                .any(|line| line == "SLURM_BITSTR_LEN=0")
        );
    }

    #[tokio::test]
    async fn live_keeps_status_and_stderr() {
        let exchange = live(argv(&["sh", "-c", "echo oops >&2; exit 3"]), TIMEOUT)
            .await
            .unwrap();
        assert_eq!((exchange.status, exchange.stderr.as_str()), (3, "oops\n"));
    }

    #[tokio::test]
    async fn live_kills_commands_past_the_timeout() {
        let started = std::time::Instant::now();
        let result = live(argv(&["sleep", "5"]), Duration::from_millis(100)).await;
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn recording(exchanges: &[Exchange]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for exchange in exchanges {
            writeln!(file, "{}", serde_json::to_string(exchange).unwrap()).unwrap();
        }
        file
    }

    fn exchange(at: i64, words: &[&str], stdout: &str) -> Exchange {
        Exchange {
            at: UnixTime(at),
            argv: argv(words),
            status: 0,
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    #[tokio::test]
    async fn replay_answers_each_command_in_order_and_drives_the_clock() {
        let path = recording(&[
            exchange(100, &["squeue"], "first"),
            exchange(101, &["sinfo"], "nodes"),
            exchange(110, &["squeue"], "second"),
        ]);
        let mut source = Source::Replay(Replay::open(path.path()).unwrap());
        assert_eq!(source.now(), UnixTime(100));
        assert_eq!(source.run(argv(&["squeue"])).await.unwrap().stdout, "first");
        assert_eq!(
            source.run(argv(&["squeue"])).await.unwrap().stdout,
            "second"
        );
        assert_eq!(source.now(), UnixTime(110));
        assert_eq!(source.run(argv(&["sinfo"])).await.unwrap().stdout, "nodes");
        assert_eq!(source.now(), UnixTime(110));
        assert!(source.run(argv(&["squeue"])).await.is_err());
    }

    #[tokio::test]
    async fn replay_answers_unrecorded_lookups_as_gone() {
        let path = recording(&[exchange(100, &["squeue"], "")]);
        let mut source = Source::Replay(Replay::open(path.path()).unwrap());
        let mut lookup = scontrol::jobs_argv();
        lookup.push("123_4".to_owned());
        let answer = source.run(lookup).await.unwrap();
        assert_eq!(answer.status, 1);
        assert!(answer.stderr.contains("Invalid job id specified"));
    }
}
