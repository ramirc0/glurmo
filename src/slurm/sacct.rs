//! What accounting holds: outcomes of jobs that left squeue unseen, and history for a backfill.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::fields::{columns, integer, is_placeholder, job_state, seconds, time, tres};
use super::hostlist::{count_ids, expand_hosts};
use glurmo_model::{
    ArrayTasks, Attempt, AttemptKey, JobId, JobState, Login, PartitionName, UnixTime,
};

/// What accounting holds for one attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// The `-o` fields of a history query, in the order [`parse_history`] reads them.
pub const HISTORY_FIELDS: [&str; 14] = [
    "JobIDRaw",
    "JobID",
    "User",
    "Partition",
    "State",
    "AllocCPUS",
    "ReqCPUS",
    "AllocTRES",
    "ReqTRES",
    "Timelimit",
    "Submit",
    "Start",
    "End",
    "NodeList",
];

/// The command for every allocation that was pending or running between `from` and `to`.
///
/// sacct reads `-S` and `-E` as local time and rejects epoch seconds. The child environment sets
/// `TZ=UTC`, so the times go in as UTC.
///
/// # Errors
///
/// Fails when a time is out of range for a calendar date.
pub fn history_argv(from: UnixTime, to: UnixTime) -> Result<Vec<String>> {
    let iso = |time: UnixTime| -> Result<String> {
        Ok(jiff::Timestamp::from_second(time.0)?
            .strftime("%Y-%m-%dT%H:%M:%S")
            .to_string())
    };
    Ok(["sacct", "-a", "-X", "-D", "-n", "-P", "-o"]
        .map(String::from)
        .into_iter()
        .chain([
            HISTORY_FIELDS.join(","),
            "-S".into(),
            iso(from)?,
            "-E".into(),
            iso(to)?,
        ])
        .collect())
}

/// Parses the output of [`history_argv`] into attempts. Accounting keeps no reasons or names, so those stay empty.
///
/// # Errors
///
/// Fails on a line with too few columns or a value that does not parse.
pub fn parse_history(stdout: &str) -> Result<Vec<Attempt>> {
    stdout
        .lines()
        .map(|line| accounted(line).with_context(|| format!("sacct line {line:?}")))
        .collect()
}

/// Reads one allocation. A pending job has no allocation yet, so its CPUs, memory and GPUs come from the request.
fn accounted(line: &str) -> Result<Attempt> {
    let [
        id,
        array_id,
        login,
        partitions,
        state,
        alloc_cpus,
        req_cpus,
        alloc_tres,
        req_tres,
        limit,
        submit,
        start,
        end,
        nodes,
    ] = columns(line)?;
    let state = state.split_once(' ').map_or(state, |(state, _)| state);
    let allocated = integer::<u32>(alloc_cpus)? > 0;
    let (cpus, tres) = if allocated {
        (alloc_cpus, tres(alloc_tres)?)
    } else {
        (req_cpus, tres(req_tres)?)
    };
    Ok(Attempt {
        key: AttemptKey {
            job: JobId(integer(id)?),
            submit: time(submit)?.context("no submit time")?,
        },
        array: array_tasks(array_id)?,
        login: Login(login.to_owned()),
        partitions: partitions
            .split(',')
            .map(|name| PartitionName(name.to_owned()))
            .collect(),
        state: job_state(state)?,
        state_raw: state.to_owned(),
        cpus: integer(cpus)?,
        mem_mb: tres.mem_mb,
        gpus: tres.gpus,
        time_limit_s: seconds(limit)?,
        start: time(start)?,
        end: time(end)?,
        nodes: expand_hosts(nodes)?,
        reason: None,
        name: String::new(),
    })
}

/// Reads the array part of a sacct `JobID`, such as `12_7` for one task or `12_[0-3%2]` for a pending range.
fn array_tasks(job_id: &str) -> Result<Option<ArrayTasks>> {
    let Some((base, tasks)) = job_id.split_once('_') else {
        return Ok(None);
    };
    let spec = tasks
        .strip_prefix('[')
        .and_then(|tasks| tasks.strip_suffix(']'))
        .unwrap_or(tasks);
    if is_placeholder(spec) {
        return Ok(None);
    }
    let ids = spec.split_once('%').map_or(spec, |(ids, _)| ids);
    Ok(Some(ArrayTasks {
        job: JobId(integer(base)?),
        spec: spec.to_owned(),
        count: count_ids(ids)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glurmo_model::{EndState, NodeName};

    #[test]
    fn a_stepped_array_range_counts_its_tasks() {
        let line = "1754696|1754696_[3-15:2]|kite|b200-batch|CANCELLED by 20001|0|3||billing=3,cpu=3,gres/gpu:b200=1,gres/gpu=1,mem=32G,node=1|00:30:00|1794442167|None|1794453518|None assigned";
        let attempt = parse_history(line).unwrap().remove(0);
        let array = attempt.array.unwrap();
        assert_eq!((array.spec.as_str(), array.count), ("3-15:2", 7));
    }

    #[test]
    fn history_asks_for_a_utc_window() {
        let argv = history_argv(UnixTime(1_794_614_400), UnixTime(1_794_700_800)).unwrap();
        assert_eq!(
            argv[7],
            "JobIDRaw,JobID,User,Partition,State,AllocCPUS,ReqCPUS,AllocTRES,ReqTRES,Timelimit,Submit,Start,End,NodeList"
        );
        assert_eq!(
            argv[8..],
            ["-S", "2026-11-14T00:00:00", "-E", "2026-11-15T00:00:00"]
        );
    }

    #[test]
    fn history_rows() {
        let attempts = parse_history(
            "\
3441951|3441951_[0-3]|wren|long,12hours|PENDING|0|1||billing=1,cpu=1,mem=1G,node=1|5:00|1792077150|Unknown|Unknown|None assigned
3849576|3849570_6|heron|4hours|TIMEOUT|2|2|billing=2,cpu=2,mem=16G,node=1|billing=2,cpu=2,mem=16G,node=1|2:00:00|1794259870|1794259871|1794691887|z1014
3923677|3923677|lynx|gpuh200|CANCELLED by 20001|32|32|billing=32,cpu=32,gres/gpu=4,mem=256G,node=1|billing=32,cpu=32,gres/gpu=4,mem=256G,node=1|14-00:00:00|1794524389|1794524389|1794600000|z028
",
        )
        .unwrap();

        let range = &attempts[0];
        assert_eq!(range.key.job, JobId(3_441_951));
        let tasks = range.array.as_ref().unwrap();
        assert_eq!(
            (tasks.job, tasks.spec.as_str(), tasks.count),
            (JobId(3_441_951), "0-3", 4)
        );
        assert_eq!(
            (range.state, range.cpus, range.mem_mb),
            (JobState::Pending, 1, Some(1024))
        );
        assert_eq!((range.start, range.end, range.nodes.len()), (None, None, 0));
        assert_eq!(range.partitions.len(), 2);

        let task = &attempts[1];
        assert_eq!(task.array.as_ref().unwrap().spec, "6");
        assert_eq!(task.state, JobState::Ended(EndState::Timeout));
        assert_eq!(task.end, Some(UnixTime(1_794_691_887)));
        assert_eq!(task.nodes, [NodeName("z1014".into())]);

        let gpu = &attempts[2];
        assert_eq!((gpu.array.as_ref(), gpu.cpus, gpu.gpus), (None, 32, 4));
        assert_eq!(gpu.state, JobState::Ended(EndState::Cancelled));
    }

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
3963301|1794695660|1794695900|CANCELLED by 20001
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
