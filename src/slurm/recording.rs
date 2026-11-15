//! Recordings: each call glurmo made to Slurm and what came back, as model types, one JSON line each.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::sacct::Outcome;
use super::scontrol::Placed;
use super::{Lookup, Site};
use glurmo_model::{Attempt, AttemptKey, JobId, Login, Node, Partition, Stats, UnixTime};

/// One question glurmo asks Slurm.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Call {
    /// The site's configuration.
    Config,
    /// Every job, including ones that ended within `MinJobAge`.
    Jobs,
    /// Every node, with its partitions.
    Nodes,
    /// Every partition.
    Partitions,
    /// The placement of every job the controller knows.
    Placements,
    /// One attempt's placement. `printed` is the ID as squeue prints it.
    Lookup {
        /// The attempt.
        key: AttemptKey,
        /// Its printed ID, such as `12_7` for an array task.
        printed: String,
    },
    /// What accounting holds for these jobs.
    Outcomes(Vec<JobId>),
    /// Every allocation accounting holds that was pending or running in a window.
    History {
        /// The window's start.
        from: UnixTime,
        /// The window's end.
        to: UnixTime,
    },
    /// Scheduler and RPC counters.
    Stats,
}

/// What Slurm answered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Answer {
    /// Site facts.
    Config(Site),
    /// Jobs.
    Jobs(Vec<Attempt>),
    /// Nodes.
    Nodes(Vec<Node>),
    /// Partitions.
    Partitions(Vec<Partition>),
    /// Placements.
    Placements(Vec<Placed>),
    /// One lookup.
    Lookup(Lookup),
    /// Accounting outcomes.
    Outcomes(Vec<Outcome>),
    /// Accounting history.
    History(Vec<Attempt>),
    /// Counters.
    Stats(Stats),
}

/// One call and its answer, or the error it met.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exchange {
    /// When the call started.
    pub at: UnixTime,
    /// The call.
    pub call: Call,
    /// The answer, or why there was none.
    pub answer: Result<Answer, String>,
}

/// Reads every exchange in a recording.
///
/// # Errors
///
/// Fails when the file cannot be read or a line is not an exchange.
pub fn read(path: &Path) -> Result<Vec<Exchange>> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .enumerate()
        .map(|(number, line)| {
            serde_json::from_str(&line?)
                .with_context(|| format!("{} line {}", path.display(), number + 1))
        })
        .collect()
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
    /// Fails when the exchange cannot be written.
    pub fn write(&mut self, exchange: &Exchange) -> Result<()> {
        let mut exchange = exchange.clone();
        if let Some(pseudonyms) = &mut self.pseudonyms {
            pseudonyms.rewrite(&mut exchange);
        }
        let mut line = serde_json::to_string(&exchange)?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .context("cannot write the recording")
    }
}

const ANIMALS: [&str; 24] = [
    "otter", "heron", "lynx", "wren", "ibis", "newt", "vole", "kite", "puma", "yak", "moth",
    "crane", "finch", "gecko", "hare", "mink", "orca", "quail", "seal", "tern", "viper", "wolf",
    "zebu", "bison",
];

/// Stable stand-ins for every login and job name seen so far. Nothing else a recording holds names a
/// person.
#[derive(Default)]
pub struct Pseudonyms {
    logins: HashMap<String, String>,
    names: HashMap<String, String>,
}

impl Pseudonyms {
    /// Rewrites the logins and job names in an exchange's answer.
    pub fn rewrite(&mut self, exchange: &mut Exchange) {
        if let Ok(Answer::Jobs(attempts) | Answer::History(attempts)) = &mut exchange.answer {
            for attempt in attempts {
                attempt.login = self.login(&attempt.login);
                if !attempt.name.is_empty() {
                    attempt.name = self.name(&attempt.name);
                }
            }
        }
    }

    fn login(&mut self, login: &Login) -> Login {
        let next = self.logins.len();
        Login(
            self.logins
                .entry(login.0.clone())
                .or_insert_with(|| {
                    ANIMALS
                        .get(next)
                        .map_or_else(|| format!("user{next}"), |animal| (*animal).to_owned())
                })
                .clone(),
        )
    }

    fn name(&mut self, name: &str) -> String {
        let next = self.names.len();
        self.names
            .entry(name.to_owned())
            .or_insert_with(|| format!("job{}", next + 1))
            .clone()
    }
}

/// Answers calls from a recording.
///
/// Each call gets its recorded answers in order. A lookup with no answer left is
/// [`Lookup::Gone`], as a job that has left the controller would be.
pub struct Replay {
    answers: HashMap<Call, VecDeque<Exchange>>,
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
        Ok(Self::new(read(path)?))
    }

    /// Replays `exchanges`. The clock starts at the first one.
    pub fn new(exchanges: Vec<Exchange>) -> Self {
        let now = exchanges
            .first()
            .map_or(UnixTime(0), |exchange| exchange.at);
        let mut answers: HashMap<Call, VecDeque<Exchange>> = HashMap::new();
        for exchange in exchanges {
            answers
                .entry(exchange.call.clone())
                .or_default()
                .push_back(exchange);
        }
        Self {
            answers,
            now,
            paced: false,
        }
    }

    /// Makes the replay wait out the recorded time between ticks, so a browser sees it unfold live.
    pub fn paced(self) -> Self {
        Self {
            paced: true,
            ..self
        }
    }

    /// The time of the last answer given.
    pub fn now(&self) -> UnixTime {
        self.now
    }

    /// Waits until `time`, at once unless paced.
    pub async fn sleep_until(&mut self, time: UnixTime) {
        if self.paced {
            let wait = u64::try_from(time.0 - self.now.0).unwrap_or(0);
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }
        self.now = self.now.max(time);
    }

    /// The next recorded answer to `call`.
    ///
    /// # Errors
    ///
    /// Gives the recorded error, or [`Exhausted`] when no answer is left.
    pub fn answer(&mut self, call: &Call) -> Result<Answer> {
        if let Some(exchange) = self.answers.get_mut(call).and_then(VecDeque::pop_front) {
            self.now = self.now.max(exchange.at);
            return exchange.answer.map_err(anyhow::Error::msg);
        }
        if let Call::Lookup { .. } = call {
            return Ok(Answer::Lookup(Lookup::Gone));
        }
        Err(Exhausted(format!("{call:?}")).into())
    }
}

/// The error a replay gives once it has no answer left for a call.
#[derive(Debug)]
pub struct Exhausted(String);

impl std::fmt::Display for Exhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "the recording has no answer left for {}", self.0)
    }
}

impl std::error::Error for Exhausted {}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(at: i64, call: Call, answer: Answer) -> Exchange {
        Exchange {
            at: UnixTime(at),
            call,
            answer: Ok(answer),
        }
    }

    #[tokio::test]
    async fn replay_answers_each_call_in_order_and_drives_the_clock() {
        let stats = |jobs| {
            Answer::Stats(Stats {
                jobs_started: Some(jobs),
                ..Stats::default()
            })
        };
        let mut replay = Replay::new(vec![
            exchange(100, Call::Stats, stats(1)),
            exchange(101, Call::Nodes, Answer::Nodes(Vec::new())),
            exchange(110, Call::Stats, stats(2)),
        ]);
        assert_eq!(replay.now(), UnixTime(100));
        assert_eq!(replay.answer(&Call::Stats).unwrap(), stats(1));
        assert_eq!(replay.answer(&Call::Stats).unwrap(), stats(2));
        assert_eq!(replay.now(), UnixTime(110));
        assert!(replay.answer(&Call::Nodes).is_ok());
        assert!(replay.answer(&Call::Stats).unwrap_err().is::<Exhausted>());
        replay.sleep_until(UnixTime(200)).await;
        assert_eq!(replay.now(), UnixTime(200));
    }

    #[tokio::test(start_paused = true)]
    async fn a_paced_replay_waits_out_the_time_between_ticks() {
        let mut replay =
            Replay::new(vec![exchange(100, Call::Jobs, Answer::Jobs(Vec::new()))]).paced();
        let started = tokio::time::Instant::now();
        replay.sleep_until(UnixTime(130)).await;
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(30));
        replay.sleep_until(UnixTime(120)).await;
        assert_eq!(
            started.elapsed(),
            std::time::Duration::from_secs(30),
            "the past costs no time"
        );
    }

    #[test]
    fn replay_answers_unrecorded_lookups_as_gone_and_gives_back_errors() {
        let mut replay = Replay::new(vec![Exchange {
            at: UnixTime(100),
            call: Call::Jobs,
            answer: Err("slurmctld did not answer".into()),
        }]);
        let lookup = Call::Lookup {
            key: AttemptKey {
                job: JobId(1),
                submit: UnixTime(5),
            },
            printed: "1".into(),
        };
        assert_eq!(
            replay.answer(&lookup).unwrap(),
            Answer::Lookup(Lookup::Gone)
        );
        let error = replay.answer(&Call::Jobs).unwrap_err();
        assert_eq!(error.to_string(), "slurmctld did not answer");
    }

    #[test]
    fn pseudonyms_are_stable_and_cover_logins_and_names() {
        let attempt = |login: &str, name: &str| {
            let mut attempt = crate::slurm::squeue::parse(
                "7|7|N/A|x|4hours|PENDING|4|cpu=4,mem=4G,node=1,billing=4|1:00:00|1794244136|N/A|N/A||Priority|n",
            )
            .unwrap()
            .remove(0);
            attempt.login = Login(login.into());
            attempt.name = name.into();
            attempt
        };
        let mut exchange = Exchange {
            at: UnixTime(1),
            call: Call::Jobs,
            answer: Ok(Answer::Jobs(vec![
                attempt("ramirezc", "secret"),
                attempt("someone", "secret"),
                attempt("ramirezc", "other"),
            ])),
        };
        Pseudonyms::default().rewrite(&mut exchange);
        let Ok(Answer::Jobs(jobs)) = exchange.answer else {
            panic!("not jobs")
        };
        let pairs: Vec<(&str, &str)> = jobs
            .iter()
            .map(|job| (job.login.0.as_str(), job.name.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [("otter", "job1"), ("heron", "job1"), ("otter", "job2")]
        );
    }
}
