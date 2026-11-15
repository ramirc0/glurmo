//! Every job squeue lists, including ones that ended within `MinJobAge`.

use anyhow::{Context, Result};

use super::fields::{
    columns, format_spec, integer, is_placeholder, job_state, seconds, time, tres,
};
use super::hostlist::{count_ids, expand_hosts};
use glurmo_model::{ArrayTasks, Attempt, AttemptKey, JobId, Login, PartitionName};

/// The `-O` fields, in the order [`parse`] reads them. Free text goes last.
pub const FIELDS: [&str; 15] = [
    "JobID",
    "ArrayJobID",
    "ArrayTaskID",
    "UserName",
    "Partition",
    "State",
    "NumCPUs",
    "tres-alloc",
    "TimeLimit",
    "SubmitTime",
    "StartTime",
    "EndTime",
    "NodeList",
    "Reason",
    "Name",
];

/// The command. `-r` stays off, because one array could expand to 500,000 rows.
pub fn argv() -> Vec<String> {
    ["squeue", "-h", "--all", "-t", "all", "-O"]
        .map(String::from)
        .into_iter()
        .chain([format_spec(&FIELDS)])
        .collect()
}

/// Parses the output of [`argv`].
///
/// # Errors
///
/// Fails on a line with too few columns or a value that does not parse.
pub fn parse(stdout: &str) -> Result<Vec<Attempt>> {
    stdout
        .lines()
        .map(|line| attempt(line).with_context(|| format!("squeue line {line:?}")))
        .collect()
}

fn attempt(line: &str) -> Result<Attempt> {
    let [
        id,
        array_job,
        array_task,
        login,
        partitions,
        state,
        cpus,
        tres_alloc,
        limit,
        submit,
        start,
        end,
        nodes,
        reason,
        name,
    ] = columns(line)?;
    let array = if is_placeholder(array_task) {
        None
    } else {
        let ids = array_task
            .split_once('%')
            .map_or(array_task, |(ids, _)| ids);
        Some(ArrayTasks {
            job: JobId(integer(array_job)?),
            spec: array_task.to_owned(),
            count: count_ids(ids)?,
        })
    };
    let tres = tres(tres_alloc)?;
    Ok(Attempt {
        key: AttemptKey {
            job: JobId(integer(id)?),
            submit: time(submit)?.context("no submit time")?,
        },
        array,
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
        reason: (!is_placeholder(reason)).then(|| reason.to_owned()),
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glurmo_model::{EndState, JobState, NodeName, UnixTime};

    const LIVE: &str = "\
3700690|3700690|N/A|otter|12hours|PENDING|8|cpu=8,mem=200G,node=1,billing=8|12:00:00|1793370192|N/A|N/A||DependencyNeverSatisfied|job1
3848210|3848210|174|heron|4hours|COMPLETED|2|cpu=2,mem=16G,billing=2|2:00:00|1794244136|1794253286|1794253868|z1014|None|job2
3848267|3848210|31|heron|4hours|COMPLETING|2|cpu=2,mem=16G,node=1,billing=2|2:00:00|1794244136|1794250847|1794258065|z1080|TimeLimit|job2
3923677|3923677|N/A|lynx|gpuh200|RUNNING|32|cpu=32,mem=256G,node=1,billing=32,gres/gpu=4|14-00:00:00|1794524389|1794524389|1795733989|z028|None|job3
3441951|3441951|0-3|wren|long|PENDING|1|cpu=2,mem=1G,node=1,billing=2|5:00|1792077150|1795868947|1795869247||Resources|job4
";

    #[test]
    fn spec_pads_nothing_and_ends_in_free_text() {
        let spec = argv().pop().unwrap();
        assert!(spec.starts_with("JobID:0|,ArrayJobID:0|,"));
        assert!(spec.ends_with("Reason:0|,Name:0"));
    }

    #[test]
    fn live_rows() {
        let jobs = parse(LIVE).unwrap();
        assert_eq!(jobs.len(), 5);

        let pending = &jobs[0];
        assert_eq!(pending.state, JobState::Pending);
        assert_eq!(pending.array, None);
        assert_eq!((pending.start, pending.end), (None, None));
        assert_eq!(pending.nodes, Vec::new());
        assert_eq!(pending.mem_mb, Some(204_800));
        assert_eq!(pending.reason.as_deref(), Some("DependencyNeverSatisfied"));

        let gpu = &jobs[3];
        assert_eq!(gpu.state, JobState::Running);
        assert_eq!((gpu.cpus, gpu.gpus), (32, 4));
        assert_eq!(gpu.time_limit_s, Some(14 * 86_400));
        assert_eq!(gpu.nodes, [NodeName("z028".into())]);
        assert_eq!(gpu.reason, None);
    }

    #[test]
    fn array_task_that_reuses_the_base_id() {
        let jobs = parse(LIVE).unwrap();
        let last = &jobs[1];
        assert_eq!(last.key.job, JobId(3_848_210));
        assert_eq!(last.state, JobState::Ended(EndState::Completed));
        let array = last.array.as_ref().unwrap();
        assert_eq!(
            (array.job, array.spec.as_str(), array.count),
            (JobId(3_848_210), "174", 1)
        );

        let completing = &jobs[2];
        assert_eq!(completing.key.job, JobId(3_848_267));
        assert_eq!(completing.state, JobState::Completing);
        assert_eq!(completing.end, Some(UnixTime(1_794_258_065)));
    }

    #[test]
    fn pending_array_range_is_one_row_with_a_task_count() {
        let jobs = parse(LIVE).unwrap();
        let range = jobs[4].array.as_ref().unwrap();
        assert_eq!((range.spec.as_str(), range.count), ("0-3", 4));
        assert_eq!(jobs[4].start, Some(UnixTime(1_795_868_947)));
    }

    #[test]
    fn throttled_range_counts_tasks_without_the_throttle() {
        let line = "9|9|45-200,300%48|kite|4hours|PENDING|2|cpu=2,mem=16G,node=1,billing=2|2:00:00|1794244136|N/A|N/A||JobArrayTaskLimit|job5";
        let range = parse(line).unwrap().remove(0).array.unwrap();
        assert_eq!((range.spec.as_str(), range.count), ("45-200,300%48", 157));
    }

    #[test]
    fn name_keeps_its_pipes_and_partitions_split() {
        let line = "7|7|N/A|puma|4hours,12hours|PENDING|4|cpu=4,mem=4G,node=1,billing=4|1:00:00|1794244136|N/A|N/A||Priority|a | b|c";
        let job = parse(line).unwrap().remove(0);
        assert_eq!(job.name, "a | b|c");
        assert_eq!(
            job.partitions,
            [
                PartitionName("4hours".into()),
                PartitionName("12hours".into())
            ]
        );
    }

    #[test]
    fn multi_node_job_lists_every_node() {
        let line = "8|8|N/A|yak|5days|RUNNING|208|cpu=208,mem=2000000M,node=2,billing=208|5-00:00:00|1794244136|1794244136|1794676136|z[1060-1061]|None|mpi";
        let job = parse(line).unwrap().remove(0);
        assert_eq!(
            job.nodes,
            [NodeName("z1060".into()), NodeName("z1061".into())]
        );
        assert_eq!(job.mem_mb, Some(2_000_000));
    }

    #[test]
    fn short_lines_and_bad_values_fail() {
        assert!(parse("3700690|3700690|N/A").is_err());
        let bad_state = LIVE.lines().next().unwrap().replace("PENDING", "DANCING");
        assert!(parse(&bad_state).is_err());
    }
}
