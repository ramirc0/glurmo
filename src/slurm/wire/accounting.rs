//! What slurmdbd holds about jobs, as sacct reads it.

use anyhow::{Context, Result};

use super::jobs::{ArrayTasks, array_tasks, state_bits, state_name, task_set};
use super::{NO_VAL, minutes, seconds};

const TRES_CPU: u32 = 1;
const TRES_MEM: u32 = 2;

/// One allocation as accounting holds it, reduced to what glurmo reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The job ID, unique to each array task.
    pub id: u32,
    /// The array's base job ID, or 0 outside an array.
    pub array_base: u32,
    /// The array task ID, or [`NO_VAL`] for none.
    pub array_task: u32,
    /// The tasks a pending array record stands for, ascending.
    pub array_range: Option<Vec<u32>>,
    /// How many of the array's tasks may run at once, or 0 for no throttle.
    pub array_max_tasks: u32,
    /// The UID of its owner.
    pub user_id: u32,
    /// The owner's login, when accounting holds it.
    pub user: Option<String>,
    /// The partitions it may run in, separated by commas.
    pub partition: Option<String>,
    /// The state as sacct prints it, such as `COMPLETED`.
    pub state: &'static str,
    /// CPUs requested.
    pub req_cpus: u32,
    /// The TRES it held, as `id=count` pairs such as `1=8,2=16000`. See [`Tres::counts`].
    pub tres_alloc: Option<String>,
    /// The TRES it asked for, in the same form.
    pub tres_req: Option<String>,
    /// The time limit in minutes. [`INFINITE`](super::INFINITE) is no limit, and
    /// [`NO_VAL`] is a limit not set.
    pub time_limit: u32,
    /// When it was submitted. 0 is none.
    pub submit: i64,
    /// When it started. 0 is none, and so is [`NO_VAL`], which slurmdbd stores for a job cancelled
    /// while pending.
    pub start: i64,
    /// When it ended. 0 is none.
    pub end: i64,
    /// Its nodes, as a hostlist such as `z[1001-1004]`.
    pub nodes: Option<String>,
}

/// The allocation, reduced to what glurmo reads.
pub fn record(job: &slurm_client::JobRecord) -> Record {
    let tres =
        |tres: &Option<slurm_client::Tres>| tres.as_ref().map(|tres| tres.as_str().to_owned());
    Record {
        id: job.id.0,
        array_base: job.array_job_id.map_or(0, |id| id.0),
        array_task: job.array_task_id.unwrap_or(NO_VAL),
        array_range: task_set(&job.array_tasks),
        array_max_tasks: job.array_max_tasks.unwrap_or(0),
        user_id: job.user_id.0,
        user: job.user_name.clone(),
        partition: job.partition.clone(),
        state: state_name(state_bits(job.state)),
        req_cpus: job.required_cpus,
        tres_alloc: tres(&job.tres_alloc),
        tres_req: tres(&job.tres_req),
        time_limit: minutes(job.time_limit),
        submit: seconds(job.submit_time),
        start: seconds(job.start_time),
        end: seconds(job.end_time),
        nodes: Some(job.nodes.as_str().to_owned()),
    }
}

/// The TRES IDs glurmo reads in accounting's `id=count` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tres {
    /// The ID of `gres/gpu`, if the site defines it.
    pub gpu: Option<u32>,
}

impl Tres {
    /// Finds the ID of `gres/gpu` among slurmdbd's trackable resources.
    pub fn among(kinds: &[slurm_client::TresKind]) -> Self {
        let gpu = kinds
            .iter()
            .find(|kind| kind.kind == "gres" && kind.name.as_deref() == Some("gpu"))
            .map(|kind| kind.id);
        Self { gpu }
    }

    /// Reads a TRES string such as `1=8,2=16000,4=1,1001=2`.
    ///
    /// # Errors
    ///
    /// Fails on an entry that is not `id=count`.
    pub fn counts(self, text: Option<&str>) -> Result<Counts> {
        let mut counts = Counts::default();
        for entry in text
            .unwrap_or_default()
            .split(',')
            .filter(|e| !e.is_empty())
        {
            let (id, count) = entry
                .split_once('=')
                .with_context(|| format!("bad TRES entry {entry:?}"))?;
            let (id, count): (u32, u64) = (id.parse()?, count.parse()?);
            match id {
                TRES_CPU => counts.cpus = Some(count),
                TRES_MEM => counts.mem_mb = Some(count),
                _ if Some(id) == self.gpu => counts.gpus = count,
                _ => {}
            }
        }
        Ok(counts)
    }
}

/// The CPUs, memory and GPUs in a TRES string.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// CPUs, if the string counts them.
    pub cpus: Option<u64>,
    /// Memory in MB, if the string counts it.
    pub mem_mb: Option<u64>,
    /// GPUs of any type.
    pub gpus: u64,
}

impl Record {
    /// The array tasks the record stands for: a pending range, one task, or none.
    pub fn array_tasks(&self) -> Option<ArrayTasks> {
        array_tasks(
            self.array_base,
            self.array_task,
            self.array_range.as_deref(),
            self.array_max_tasks,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tres_strings_give_cpus_memory_and_gpus() {
        let counts = Tres { gpu: Some(1001) }
            .counts(Some("1=8,2=16000,4=1,1001=2"))
            .unwrap();
        assert_eq!(
            (counts.cpus, counts.mem_mb, counts.gpus),
            (Some(8), Some(16_000), 2)
        );
        assert!(Tres { gpu: None }.counts(Some("1:8")).is_err());
    }

    #[test]
    fn a_record_stands_for_one_task_a_pending_range_or_none() {
        let task = Record {
            id: 9,
            array_base: 5,
            array_task: 3,
            array_range: None,
            array_max_tasks: 0,
            user_id: 20_001,
            user: Some("heron".into()),
            partition: Some("long".into()),
            state: "COMPLETED",
            req_cpus: 2,
            tres_alloc: None,
            tres_req: None,
            time_limit: 60,
            submit: 1_000,
            start: 1_100,
            end: 1_900,
            nodes: None,
        };
        let tasks = |spec: &str, count| ArrayTasks {
            job: 5,
            spec: spec.into(),
            count,
        };
        assert_eq!(task.array_tasks(), Some(tasks("3", 1)));
        let range = Record {
            array_task: NO_VAL,
            array_range: Some(vec![0, 2, 3]),
            array_max_tasks: 2,
            ..task.clone()
        };
        assert_eq!(range.array_tasks(), Some(tasks("0,2-3%2", 3)));
        let alone = Record {
            array_task: NO_VAL,
            ..task
        };
        assert_eq!(alone.array_tasks(), None);
    }
}
