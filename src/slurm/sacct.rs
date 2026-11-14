//! Outcomes of jobs that left squeue before the sampler saw how they ended.

use anyhow::{Context, Result};

use super::fields::{columns, integer, job_state, time};
use crate::model::{AttemptKey, JobId, JobState, UnixTime};

/// What accounting holds for one attempt.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The job ID and submit time.
    pub key: AttemptKey,
    /// When it ended, if it has.
    pub end: Option<UnixTime>,
    /// Its state in accounting. A runaway job that the controller forgot still reads as running.
    pub state: JobState,
}

/// The command for the allocations of `jobs`. `-D` keeps every requeued attempt.
pub fn argv(jobs: &[JobId]) -> Vec<String> {
    let ids = jobs
        .iter()
        .map(|job| job.0.to_string())
        .collect::<Vec<_>>()
        .join(",");
    [
        "sacct",
        "-X",
        "-D",
        "-n",
        "-P",
        "-o",
        "JobIDRaw,Submit,End,State",
        "-j",
    ]
    .map(String::from)
    .into_iter()
    .chain([ids])
    .collect()
}

/// Parses the output of [`argv`].
///
/// # Errors
///
/// Fails on a line with too few columns or a value that does not parse.
pub fn parse(stdout: &str) -> Result<Vec<Outcome>> {
    stdout
        .lines()
        .map(|line| outcome(line).with_context(|| format!("sacct line {line:?}")))
        .collect()
}

fn outcome(line: &str) -> Result<Outcome> {
    let [id, submit, end, state] = columns(line)?;
    let state = state.split_once(' ').map_or(state, |(state, _)| state);
    Ok(Outcome {
        key: AttemptKey {
            job: JobId(integer(id)?),
            submit: time(submit)?.context("no submit time")?,
        },
        end: time(end)?,
        state: job_state(state)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EndState;

    #[test]
    fn asks_for_raw_ids_in_one_call() {
        let argv = argv(&[JobId(3_848_267), JobId(3_962_731)]);
        assert_eq!(argv.last().unwrap(), "3848267,3962731");
        assert!(argv.contains(&"-D".to_owned()));
    }

    #[test]
    fn live_rows() {
        let outcomes = parse(
            "\
3848267|1794244136|1794258065|TIMEOUT
3962731|1794695573|1794695573|FAILED
3963300|1794695660|Unknown|RUNNING
3963301|1794695660|1794695900|CANCELLED by 1514701009
",
        )
        .unwrap();
        assert_eq!(
            outcomes[0],
            Outcome {
                key: AttemptKey {
                    job: JobId(3_848_267),
                    submit: UnixTime(1_794_244_136),
                },
                end: Some(UnixTime(1_794_258_065)),
                state: JobState::Ended(EndState::Timeout),
            }
        );
        assert_eq!(
            (outcomes[2].end, outcomes[2].state),
            (None, JobState::Running)
        );
        assert_eq!(outcomes[3].state, JobState::Ended(EndState::Cancelled));
    }

    #[test]
    fn short_lines_fail() {
        assert!(parse("3848267|1794244136").is_err());
    }
}
