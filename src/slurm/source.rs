//! Where command output comes from: the live cluster, the live cluster under recording, or a recording.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::pseudonymize::Pseudonyms;
use super::scontrol;
use glurmo_model::UnixTime;

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
    /// Run commands on this host and append each exchange to a recording.
    Record(Recorder),
    /// Answer from a recording.
    Replay(Replay),
}

impl Source {
    /// Runs a command, or answers it from the recording.
    ///
    /// # Errors
    ///
    /// Fails when the command cannot start, runs past the timeout, or cannot be recorded,
    /// or when the recording holds no answer for it.
    pub async fn run(&mut self, argv: Vec<String>) -> Result<Exchange> {
        match self {
            Self::Live => live(argv, TIMEOUT).await,
            Self::Record(recorder) => {
                let exchange = live(argv, TIMEOUT).await?;
                recorder.write(&exchange)?;
                Ok(exchange)
            }
            Self::Replay(replay) => replay.answer(argv),
        }
    }

    /// The current time. A replay's clock reads the time of the last answer.
    pub fn now(&self) -> UnixTime {
        match self {
            Self::Live | Self::Record(_) => UnixTime::now(),
            Self::Replay(replay) => replay.now,
        }
    }

    /// Waits until `time`. A replay moves its clock there at once.
    pub async fn sleep_until(&mut self, time: UnixTime) {
        match self {
            Self::Live | Self::Record(_) => {
                let wait = u64::try_from(time.0 - UnixTime::now().0).unwrap_or(0);
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            Self::Replay(replay) => {
                if replay.paced {
                    let wait = u64::try_from(time.0 - replay.now.0).unwrap_or(0);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                replay.now = replay.now.max(time);
            }
        }
    }
}

/// The error a replay gives once it has no answer left for a command.
#[derive(Debug)]
pub struct Exhausted(String);

impl std::fmt::Display for Exhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the recording has no answer left for {}", self.0)
    }
}

impl std::error::Error for Exhausted {}

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

/// Appends exchanges to a recording, optionally with logins and job names rewritten.
pub struct Recorder {
    file: File,
    pseudonyms: Option<Pseudonyms>,
}

impl Recorder {
    /// Creates or truncates the recording at `path`.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be created.
    pub fn create(path: &Path, pseudonymize: bool) -> Result<Self> {
        Ok(Self {
            file: File::create(path)
                .with_context(|| format!("cannot create {}", path.display()))?,
            pseudonyms: pseudonymize.then(Pseudonyms::default),
        })
    }

    /// Appends one exchange, rewritten first if the recorder pseudonymizes.
    ///
    /// # Errors
    ///
    /// Fails when the exchange cannot be rewritten or written.
    pub fn write(&mut self, exchange: &Exchange) -> Result<()> {
        let mut exchange = exchange.clone();
        if let Some(pseudonyms) = &mut self.pseudonyms {
            pseudonyms.rewrite(&mut exchange)?;
        }
        let mut line = serde_json::to_string(&exchange)?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .context("cannot write the recording")
    }
}

/// Answers commands from a recording.
///
/// Each command gets its recorded answers in order. A lookup with no answer left gets
/// "Invalid job id specified", as a job that has left the controller would.
pub struct Replay {
    answers: HashMap<Vec<String>, VecDeque<Exchange>>,
    now: UnixTime,
    paced: bool,
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
            paced: false,
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

    /// Makes the replay wait out the recorded time between ticks, so a browser sees it unfold live.
    pub fn paced(self) -> Self {
        Self {
            paced: true,
            ..self
        }
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
        Err(Exhausted(argv.join(" ")).into())
    }
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
    async fn live_sets_epoch_times_full_bitstrings_and_utc() {
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
        assert!(exchange.stdout.lines().any(|line| line == "TZ=UTC"));
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
        let error = source.run(argv(&["squeue"])).await.unwrap_err();
        assert!(error.is::<Exhausted>());
        source.sleep_until(UnixTime(200)).await;
        assert_eq!(source.now(), UnixTime(200));
    }

    #[tokio::test(start_paused = true)]
    async fn a_paced_replay_waits_out_the_time_between_ticks() {
        let path = recording(&[exchange(100, &["squeue"], "")]);
        let mut source = Source::Replay(Replay::open(path.path()).unwrap().paced());
        let started = tokio::time::Instant::now();
        source.sleep_until(UnixTime(130)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(30));
        assert_eq!(source.now(), UnixTime(130));
        source.sleep_until(UnixTime(120)).await;
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(30),
            "the past costs no time"
        );
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
