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

/// Appends exchanges to a recording, optionally rewritten by [`Pseudonyms`].
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

/// Stable stand-ins for every login, job name and free-text reason seen so far.
///
/// A job's reason can quote an association message, such as `user=alice`, and a node's reason is
/// whatever an operator typed. Reasons that are one word of letters, digits and `_`, as Slurm's own
/// reason names are, stay. Others become `reason1`, `reason2` and so on. An error's text can quote
/// command output, so it becomes [`HIDDEN_ERROR`]. Nothing else a recording holds names a person.
#[derive(Default)]
pub struct Pseudonyms {
    logins: HashMap<String, String>,
    names: HashMap<String, String>,
    reasons: HashMap<String, String>,
}

/// What an error's text becomes in a pseudonymized recording.
pub const HIDDEN_ERROR: &str = "error text removed by --pseudonymize";

impl Pseudonyms {
    /// Rewrites the logins, job names, reasons and error text in an exchange.
    pub fn rewrite(&mut self, exchange: &mut Exchange) {
        match &mut exchange.answer {
            Ok(Answer::Jobs(attempts) | Answer::History(attempts)) => {
                for attempt in attempts {
                    attempt.login = self.login(&attempt.login);
                    if !attempt.name.is_empty() {
                        attempt.name = self.name(&attempt.name);
                    }
                    attempt.reason = attempt.reason.take().map(|reason| self.reason(reason));
                }
            }
            Ok(Answer::Nodes(nodes)) => {
                for node in nodes {
                    node.reason = node.reason.take().map(|reason| self.reason(reason));
                }
            }
            Ok(_) => {}
            Err(error) => HIDDEN_ERROR.clone_into(error),
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

    fn reason(&mut self, reason: String) -> String {
        if reason.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return reason;
        }
        let next = self.reasons.len();
        self.reasons
            .entry(reason)
            .or_insert_with(|| format!("reason{}", next + 1))
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
    speed: Option<u32>,
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
            speed: None,
        }
    }

    /// Makes the replay wait out the recorded time between ticks, divided by `speed`, so a browser
    /// sees it unfold.
    pub fn paced(self, speed: u32) -> Self {
        Self {
            speed: Some(speed.max(1)),
            ..self
        }
    }

    /// The time of the last answer given.
    pub fn now(&self) -> UnixTime {
        self.now
    }

    /// Waits until `time`, at once unless paced.
    pub async fn sleep_until(&mut self, time: UnixTime) {
        if let Some(speed) = self.speed {
            let wait = u64::try_from(time.0 - self.now.0).unwrap_or(0);
            let wait = std::time::Duration::from_secs(wait) / speed;
            tokio::time::sleep(wait).await;
        }
        self.now = self.now.max(time);
    }

    /// Whether an answer to `call` is left.
    pub fn holds(&self, call: &Call) -> bool {
        self.answers
            .get(call)
            .is_some_and(|answers| !answers.is_empty())
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
            Replay::new(vec![exchange(100, Call::Jobs, Answer::Jobs(Vec::new()))]).paced(1);
        let started = tokio::time::Instant::now();
        replay.sleep_until(UnixTime(130)).await;
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(30));
        replay.sleep_until(UnixTime(120)).await;
        assert_eq!(
            started.elapsed(),
            std::time::Duration::from_secs(30),
            "the past costs no time"
        );
        let mut fast =
            Replay::new(vec![exchange(100, Call::Jobs, Answer::Jobs(Vec::new()))]).paced(10);
        let started = tokio::time::Instant::now();
        fast.sleep_until(UnixTime(130)).await;
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(3));
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
                attempt("alice", "secret"),
                attempt("someone", "secret"),
                attempt("alice", "other"),
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

    #[test]
    fn pseudonyms_cover_free_text_in_every_carrier() {
        let mut attempt = crate::slurm::squeue::parse(
            "7|7|N/A|alice|4hours|PENDING|4|cpu=4,mem=4G,node=1,billing=4|1:00:00|1794244136|N/A|N/A||Priority|secret",
        )
        .unwrap()
        .remove(0);
        let mut held = attempt.clone();
        held.reason = Some("AssocMaxJobsLimit user=alice".into());
        attempt.reason = Some("Priority".into());
        let node = Node {
            reason: Some("alice broke secret".into()),
            ..crate::slurm::sinfo::parse(
                "z1014|30mins*|idle|0/88/0/88|0.19|500000|0|254476|(null)|gpu:0|broadwell|2|22|none",
            )
            .unwrap()
            .remove(0)
        };
        let exchanges = [
            (Call::Jobs, Ok(Answer::Jobs(vec![attempt, held.clone()]))),
            (
                Call::History {
                    from: UnixTime(0),
                    to: UnixTime(1),
                },
                Ok(Answer::History(vec![held])),
            ),
            (Call::Nodes, Ok(Answer::Nodes(vec![node.clone(), node]))),
            (
                Call::Placements,
                Err("scontrol job \"JobName=secret UserId=alice(1)\": bad".into()),
            ),
        ];
        let mut pseudonyms = Pseudonyms::default();
        let mut texts = Vec::new();
        for (call, answer) in exchanges {
            let mut exchange = Exchange {
                at: UnixTime(1),
                call,
                answer,
            };
            pseudonyms.rewrite(&mut exchange);
            texts.push(serde_json::to_string(&exchange).unwrap());
        }
        for text in &texts {
            assert!(
                !text.contains("alice") && !text.contains("secret"),
                "{text}"
            );
        }
        assert!(texts[0].contains("\"Priority\""), "{}", texts[0]);
        assert!(texts[0].contains("\"reason1\"") && texts[1].contains("\"reason1\""));
        assert_eq!(texts[2].matches("\"reason2\"").count(), 2, "{}", texts[2]);
    }
}
