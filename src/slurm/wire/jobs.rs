//! Two views of a job listing: the jobs as squeue lists them, and the cores each one holds as
//! `scontrol -d show job` reports them.

use super::{NO_VAL, minutes, seconds};

/// The tasks of a job array that one record stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayTasks {
    /// The array's base job ID.
    pub job: u32,
    /// Task IDs as squeue prints them, such as `174` or `0-3,8%2`.
    pub spec: String,
    /// How many tasks the spec covers.
    pub count: u32,
}

/// A job as squeue lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// The job ID, unique to each array task.
    pub id: u32,
    /// The array tasks this record stands for, if the job is part of an array.
    pub array: Option<ArrayTasks>,
    /// The UID of its owner.
    pub user_id: u32,
    /// The controller's name for that UID, which can differ from the local account database's.
    pub user_name: Option<String>,
    /// The partitions it may run in, separated by commas.
    pub partition: String,
    /// The state as squeue prints it, such as `RUNNING`.
    pub state: &'static str,
    /// Why it waits or how it ended, as squeue prints it. `None` when there is no reason.
    pub reason: String,
    /// CPUs allocated, or requested while pending.
    pub cpus: u32,
    /// The TRES it holds, or asked for while it holds none, such as `cpu=4,mem=16G,gres/gpu=2`.
    pub tres: String,
    /// The time limit in minutes. [`INFINITE`](super::INFINITE) is no limit, and
    /// [`NO_VAL`] is a limit not set.
    pub time_limit: u32,
    /// When it was submitted, or 0 when the controller leaves it out, as 25.11 does for a job
    /// without details.
    pub submit: i64,
    /// When it started, or the controller's estimate while pending. 0 is none.
    pub start: i64,
    /// When it ended or must end. 0 is none.
    pub end: i64,
    /// Its nodes, as a hostlist such as `z[1001-1004]`.
    pub nodes: String,
    /// The job name.
    pub name: String,
}

/// The cores a job holds, as `scontrol -d show job` reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// The job ID.
    pub id: u32,
    /// When it started. 0 is none.
    pub start: i64,
    /// The state as scontrol prints it, such as `RUNNING`.
    pub state: &'static str,
    /// The hosts it holds cores on, as a hostlist. Empty for a job that holds none.
    pub hosts: String,
    /// What it holds on each of `hosts`, in their order.
    pub cells: Vec<HostCells>,
}

/// What a job holds on one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCells {
    /// Its cores, counted from the host's first core.
    pub cores: Vec<u32>,
    /// Its GRES, as a list such as `gpu:a100:2(IDX:0,2)`. Empty when it holds none.
    pub gres: String,
    /// Its memory, in MB.
    pub memory_mb: u64,
}

/// One job as the controller holds it, reduced to what glurmo reads.
#[derive(Debug, Clone, PartialEq)]
struct Raw {
    id: u32,
    array_base: u32,
    array_task: u32,
    array_tasks: Option<Vec<u32>>,
    array_max_tasks: u32,
    /// Whether pending tasks were folded into this record, which squeue then spells as plain ranges.
    combined: bool,
    user_id: u32,
    user_name: Option<String>,
    state: u32,
    state_reason: u32,
    state_desc: Option<String>,
    time_limit: u32,
    submit: i64,
    start: i64,
    end: i64,
    nodes: Option<String>,
    partition: Option<String>,
    resources: Option<Resources>,
    gres_detail: Vec<String>,
    name: Option<String>,
    cpus: u32,
    tres_alloc: Option<String>,
    tres_req: Option<String>,
}

/// The cores a job holds, from its `job_resources`.
#[derive(Debug, Clone, PartialEq)]
struct Resources {
    hosts: String,
    memory_mb: Vec<u64>,
    cores_per_host: Vec<(u32, u32)>,
    cores: Vec<u32>,
}

/// Two views of a job listing. The first lists the jobs as squeue does, with pending tasks that the
/// controller split out of an array folded back into the array's record. The second holds the cores
/// of every record.
pub fn views(jobs: &[slurm_client::Job]) -> (Vec<Job>, Vec<Placed>) {
    let mut records: Vec<Raw> = jobs.iter().map(raw).collect();
    let placed = records.iter().map(Raw::placed).collect();
    combine_pending_arrays(&mut records);
    (records.iter().map(Raw::listed).collect(), placed)
}

fn raw(job: &slurm_client::Job) -> Raw {
    Raw {
        id: job.id.0,
        array_base: job.array_job_id.map_or(0, |id| id.0),
        array_task: job.array_task_id.unwrap_or(NO_VAL),
        array_tasks: task_set(&job.array_tasks),
        array_max_tasks: job.array_max_tasks.unwrap_or(0),
        combined: false,
        user_id: job.user_id.0,
        user_name: job.user_name.clone(),
        state: state_bits(job.state),
        state_reason: job.state_reason.0,
        state_desc: job.state_description.clone(),
        time_limit: minutes(job.time_limit),
        submit: seconds(job.submit_time),
        start: seconds(job.start_time),
        end: seconds(job.end_time),
        nodes: Some(job.nodes.as_str().to_owned()),
        partition: job.partition.clone(),
        resources: job.resources.as_ref().map(|held| Resources {
            hosts: held.nodes.as_str().to_owned(),
            memory_mb: held.memory_allocated.clone(),
            cores_per_host: held
                .sockets_per_node
                .iter()
                .zip(&held.cores_per_socket)
                .zip(&held.socket_core_reps)
                .map(|((&sockets, &cores), &repeat)| {
                    (u32::from(sockets) * u32::from(cores), repeat)
                })
                .collect(),
            cores: held.cores.clone(),
        }),
        gres_detail: job
            .gres_detail
            .iter()
            .map(|gres| gres.as_str().to_owned())
            .collect(),
        name: job.name.clone(),
        cpus: job.cpus,
        tres_alloc: job.tres_alloc.as_ref().map(|tres| tres.as_str().to_owned()),
        tres_req: job.tres_req.as_ref().map(|tres| tres.as_str().to_owned()),
    }
}

/// The task IDs in a record's ranges, ascending. `None` when the record stands for no range.
pub(super) fn task_set(ranges: &[std::ops::RangeInclusive<u32>]) -> Option<Vec<u32>> {
    (!ranges.is_empty()).then(|| ranges.iter().cloned().flatten().collect())
}

/// A job state as Slurm numbers it: the base state in the low byte, then the flags.
pub(super) fn state_bits(state: slurm_client::JobState) -> u32 {
    use slurm_client::JobBaseState as Base;
    let base = match state.base {
        Base::Pending => 0,
        Base::Running => 1,
        Base::Suspended => 2,
        Base::Completed => 3,
        Base::Cancelled => 4,
        Base::Failed => 5,
        Base::Timeout => 6,
        Base::NodeFail => 7,
        Base::Preempted => 8,
        Base::BootFail => 9,
        Base::Deadline => 10,
        Base::OutOfMemory => 11,
        Base::Other(other) => other,
        _ => 0xff,
    };
    base | state.flags.bits()
}

impl Raw {
    /// The job as squeue lists it.
    fn listed(&self) -> Job {
        Job {
            id: self.id,
            array: self.array(),
            user_id: self.user_id,
            user_name: self.user_name.clone(),
            partition: self.partition.clone().unwrap_or_default(),
            state: state_name(self.state),
            reason: match &self.state_desc {
                Some(desc) => desc.clone(),
                None => reason_name(self.state_reason),
            },
            cpus: self.cpus,
            tres: self
                .tres_alloc
                .clone()
                .or_else(|| self.tres_req.clone())
                .unwrap_or_default(),
            time_limit: self.time_limit,
            submit: self.submit,
            start: self.start,
            end: self.end,
            nodes: self.nodes.clone().unwrap_or_default(),
            name: self.name.clone().unwrap_or_default(),
        }
    }

    fn array(&self) -> Option<ArrayTasks> {
        let mut tasks = array_tasks(
            self.array_base,
            self.array_task,
            self.array_tasks.as_deref(),
            self.array_max_tasks,
        );
        if let (true, Some(tasks), Some(set)) = (self.combined, &mut tasks, &self.array_tasks) {
            tasks.spec = ranges(set);
        }
        tasks
    }

    /// The job's cores, GRES and memory on each host, as `scontrol -d` reports them.
    fn placed(&self) -> Placed {
        let (hosts, cells) = self.cells();
        Placed {
            id: self.id,
            start: self.start,
            state: state_name(self.state),
            hosts,
            cells,
        }
    }

    /// Splits the core bitmap, which runs over the job's hosts in order, at each host's width.
    fn cells(&self) -> (String, Vec<HostCells>) {
        let held = self
            .resources
            .as_ref()
            .and_then(|resources| Some((resources, *resources.cores.last()?)));
        let Some((resources, last)) = held else {
            return (String::new(), Vec::new());
        };
        let widths = resources
            .cores_per_host
            .iter()
            .flat_map(|&(width, repeat)| std::iter::repeat_n(width, repeat as usize));
        let mut cells = Vec::new();
        let mut offset = 0;
        for (i, width) in widths.enumerate() {
            cells.push(HostCells {
                cores: resources
                    .cores
                    .iter()
                    .filter(|&&core| core >= offset && core < offset + width)
                    .map(|core| core - offset)
                    .collect(),
                gres: self.gres_detail.get(i).cloned().unwrap_or_default(),
                memory_mb: resources.memory_mb.get(i).copied().unwrap_or(0),
            });
            offset += width;
            if offset > last {
                break;
            }
        }
        (resources.hosts.clone(), cells)
    }
}

/// Folds pending tasks that the controller split out of an array back into the array's pending
/// record, as squeue's `_combine_pending_array_tasks` does unless asked for `-r`. A task folds in when
/// its name and partition match, and its reason joins the record's.
fn combine_pending_arrays(jobs: &mut Vec<Raw>) {
    let pending = |job: &Raw| base_state(job.state) == JOB_PENDING;
    let mut folded = vec![false; jobs.len()];
    for i in 0..jobs.len() {
        if folded[i] || !pending(&jobs[i]) || jobs[i].array_tasks.is_none() {
            continue;
        }
        for j in 0..jobs.len() {
            let (record, task) = (&jobs[i], &jobs[j]);
            if i == j
                || folded[j]
                || !pending(task)
                || task.array_base != record.array_base
                || task.array_task == NO_VAL
                || task.name != record.name
                || task.partition != record.partition
            {
                continue;
            }
            let (reason, task_id) = (task.state_reason, task.array_task);
            let record = &mut jobs[i];
            if record.state_reason != reason {
                let desc = record
                    .state_desc
                    .get_or_insert_with(|| reason_name(record.state_reason));
                let added = reason_name(reason);
                if !desc.contains(&added) {
                    desc.push(',');
                    desc.push_str(&added);
                }
            }
            if let Some(set) = &mut record.array_tasks
                && let Err(at) = set.binary_search(&task_id)
            {
                set.insert(at, task_id);
            }
            record.combined = true;
            folded[j] = true;
        }
    }
    let mut keep = folded.iter().map(|folded| !folded);
    jobs.retain(|_| keep.next().unwrap_or(true));
}

/// The array tasks a record stands for: a pending range, one task, or none.
pub(super) fn array_tasks(
    job: u32,
    task: u32,
    range: Option<&[u32]>,
    max_tasks: u32,
) -> Option<ArrayTasks> {
    match range {
        Some(set) if !set.is_empty() => Some(ArrayTasks {
            job,
            spec: task_spec(set, max_tasks),
            count: u32::try_from(set.len()).unwrap_or(u32::MAX),
        }),
        _ if task != NO_VAL => Some(ArrayTasks {
            job,
            spec: task.to_string(),
            count: 1,
        }),
        _ => None,
    }
}

/// Spells a pending range as squeue and sacct do under `SLURM_BITSTR_LEN=0`: a stepped range such as
/// `1-21:4`, else ranges such as `0-3,7`, then `%N` for a throttle.
fn task_spec(set: &[u32], max_tasks: u32) -> String {
    let spec = stepped(set).unwrap_or_else(|| ranges(set));
    if max_tasks == 0 {
        spec
    } else {
        format!("{spec}%{max_tasks}")
    }
}

/// `bit_fmt_full`: runs of set bits as `a-b`, single bits alone, joined by commas.
fn ranges(set: &[u32]) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &bit in set {
        match runs.last_mut() {
            Some((_, end)) if *end + 1 == bit => *end = bit,
            _ => runs.push((bit, bit)),
        }
    }
    runs.iter()
        .map(|&(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}-{end}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `xlate_array_task_str`'s step test: more than 5 tasks spanning more than 10 IDs, evenly spaced and
/// not adjacent, print as `first-last:step`.
fn stepped(set: &[u32]) -> Option<String> {
    let (&first, &last) = (set.first()?, set.last()?);
    if last - first <= 10 || set.len() <= 5 || set[1] == first + 1 {
        return None;
    }
    let step = set[1] - first;
    set.windows(2)
        .all(|pair| pair[1] - pair[0] == step)
        .then(|| format!("{first}-{last}:{step}"))
}

const JOB_PENDING: u32 = 0;

/// A job state without its flags.
fn base_state(state: u32) -> u32 {
    state & 0xff
}

/// `job_state_string`: a flag wins over the base state.
pub(super) fn state_name(state: u32) -> &'static str {
    const FLAGS: [(u32, &str); 13] = [
        (1 << 15, "COMPLETING"),
        (1 << 23, "STAGE_OUT"),
        (1 << 14, "CONFIGURING"),
        (1 << 24, "EXPEDITING"),
        (1 << 13, "RESIZING"),
        (1 << 10, "REQUEUED"),
        (1 << 20, "REQUEUE_FED"),
        (1 << 11, "REQUEUE_HOLD"),
        (1 << 12, "SPECIAL_EXIT"),
        (1 << 16, "STOPPED"),
        (1 << 19, "REVOKED"),
        (1 << 21, "RESV_DEL_HOLD"),
        (1 << 22, "SIGNALING"),
    ];
    const BASES: [&str; 12] = [
        "PENDING",
        "RUNNING",
        "SUSPENDED",
        "COMPLETED",
        "CANCELLED",
        "FAILED",
        "TIMEOUT",
        "NODE_FAIL",
        "PREEMPTED",
        "BOOT_FAIL",
        "DEADLINE",
        "OUT_OF_MEMORY",
    ];
    FLAGS.iter().find(|(bit, _)| state & bit != 0).map_or_else(
        || {
            BASES
                .get(base_state(state) as usize)
                .copied()
                .unwrap_or("?")
        },
        |&(_, name)| name,
    )
}

/// `job_reason_string`.
fn reason_name(reason: u32) -> String {
    slurm_client::StateReason(reason)
        .text()
        .map_or_else(|| reason.to_string(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: u32, task: u32, tasks: Option<&[u32]>, name: &str, reason: u32) -> Raw {
        Raw {
            id,
            array_base: 10,
            array_task: task,
            array_tasks: tasks.map(<[u32]>::to_vec),
            array_max_tasks: 0,
            combined: false,
            user_id: 1,
            user_name: Some("otter".into()),
            state: 0,
            state_reason: reason,
            state_desc: None,
            time_limit: 60,
            submit: 1,
            start: 0,
            end: 0,
            nodes: None,
            partition: Some("cpu".into()),
            resources: None,
            gres_detail: Vec::new(),
            name: Some(name.into()),
            cpus: 1,
            tres_alloc: None,
            tres_req: None,
        }
    }

    #[test]
    fn placement_splits_the_core_bitmap_by_host() {
        let mut job = pending(20, NO_VAL, None, "n", 0);
        assert_eq!(job.placed().hosts, "");
        assert_eq!(job.placed().cells, Vec::new());
        job.resources = Some(Resources {
            hosts: "n[1-2]".into(),
            memory_mb: vec![1_000, 2_000],
            cores_per_host: vec![(4, 2)],
            cores: vec![0, 1, 6],
        });
        job.gres_detail = vec!["gpu:1(IDX:0)".into(), String::new()];
        let cells = |cores: &[u32], gres: &str, memory_mb| HostCells {
            cores: cores.to_vec(),
            gres: gres.into(),
            memory_mb,
        };
        assert_eq!(job.placed().hosts, "n[1-2]");
        assert_eq!(
            job.placed().cells,
            [
                cells(&[0, 1], "gpu:1(IDX:0)", 1_000),
                cells(&[2], "", 2_000)
            ]
        );
    }

    #[test]
    fn pending_tasks_split_out_of_an_array_fold_back_in_as_squeue_folds_them() {
        let mut running = pending(12, 1, None, "n", 0);
        running.state = 1;
        let mut jobs = vec![
            pending(10, NO_VAL, Some(&[2, 3]), "n", 1),
            pending(11, 0, None, "n", 2),
            running,
            pending(13, 1, None, "other", 1),
        ];
        combine_pending_arrays(&mut jobs);
        let ids: Vec<u32> = jobs.iter().map(|job| job.id).collect();
        assert_eq!(ids, [10, 12, 13]);
        let range = jobs[0].listed();
        let array = range.array.unwrap();
        assert_eq!((array.spec.as_str(), array.count), ("0,2-3", 3));
        assert_eq!(range.reason, "Priority,Dependency");
    }

    #[test]
    fn task_ranges_spell_like_squeue() {
        assert_eq!(ranges(&[0, 1, 2, 3, 7, 9, 10]), "0-3,7,9-10");
        assert_eq!(stepped(&[1, 5, 9, 13, 17, 21]), Some("1-21:4".into()));
        assert_eq!(stepped(&[1, 5, 9, 13, 17]), None, "five tasks are too few");
        assert_eq!(stepped(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]), None);
    }

    #[test]
    fn flags_win_over_base_states() {
        assert_eq!(state_name(0), "PENDING");
        assert_eq!(state_name(3 | 1 << 15), "COMPLETING");
        assert_eq!(state_name(1 | 1 << 14), "CONFIGURING");
        assert_eq!(state_name(11), "OUT_OF_MEMORY");
    }

    #[test]
    fn reasons_spell_like_squeue() {
        assert_eq!(reason_name(0), "None");
        assert_eq!(reason_name(1), "Priority");
        assert_eq!(reason_name(3), "Resources");
        assert_eq!(reason_name(34), "34", "a retired reason prints its number");
        assert_eq!(reason_name(9999), "9999");
    }

    /// A running job on two hosts of different widths, with a distinct value in each field glurmo reads.
    fn running() -> Raw {
        Raw {
            state: 1,
            user_id: 1001,
            user_name: Some("root".into()),
            partition: Some("gpu,cpu".into()),
            time_limit: 90,
            submit: 100,
            start: 200,
            end: 5_600,
            nodes: Some("n[1-2]".into()),
            cpus: 6,
            tres_alloc: Some("cpu=6,mem=3000M,node=2,billing=6,gres/gpu=1".into()),
            tres_req: Some("cpu=2,mem=1G,node=1,gres/gpu=4".into()),
            resources: Some(Resources {
                hosts: "n[1-2]".into(),
                memory_mb: vec![1_000, 2_000],
                cores_per_host: vec![(4, 1), (8, 1)],
                cores: vec![0, 1, 4, 5, 6, 11],
            }),
            gres_detail: vec!["gpu:1(IDX:2)".into(), String::new()],
            ..pending(20, NO_VAL, None, "align", 0)
        }
    }

    #[test]
    fn every_job_value_lands_in_its_own_field() {
        let expected = Job {
            id: 20,
            array: None,
            user_id: 1001,
            user_name: Some("root".into()),
            partition: "gpu,cpu".into(),
            state: "RUNNING",
            reason: "None".into(),
            cpus: 6,
            tres: "cpu=6,mem=3000M,node=2,billing=6,gres/gpu=1".into(),
            time_limit: 90,
            submit: 100,
            start: 200,
            end: 5_600,
            nodes: "n[1-2]".into(),
            name: "align".into(),
        };
        assert_eq!(running().listed(), expected);
        let cells = |cores: &[u32], gres: &str, memory_mb| HostCells {
            cores: cores.to_vec(),
            gres: gres.into(),
            memory_mb,
        };
        let expected = Placed {
            id: 20,
            start: 200,
            state: "RUNNING",
            hosts: "n[1-2]".into(),
            cells: vec![
                cells(&[0, 1], "gpu:1(IDX:2)", 1_000),
                cells(&[0, 1, 2, 7], "", 2_000),
            ],
        };
        assert_eq!(running().placed(), expected);
    }

    #[test]
    fn a_pending_job_lists_what_it_requested_and_why_it_waits() {
        let waiting = Raw {
            state: 0,
            tres_alloc: None,
            state_reason: 1,
            nodes: None,
            array_task: 3,
            ..running()
        };
        let listed = waiting.listed();
        assert_eq!(listed.tres, "cpu=2,mem=1G,node=1,gres/gpu=4");
        assert_eq!(
            (listed.state, listed.reason.as_str()),
            ("PENDING", "Priority")
        );
        assert_eq!(listed.nodes, "");
        let expected = ArrayTasks {
            job: 10,
            spec: "3".into(),
            count: 1,
        };
        assert_eq!(listed.array, Some(expected));
        let described = Raw {
            state_desc: Some("launch failed requeued held".into()),
            ..waiting
        };
        assert_eq!(described.listed().reason, "launch failed requeued held");
    }
}
