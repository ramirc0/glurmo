//! Jobs and their placement, from `RESPONSE_JOB_INFO`.

use anyhow::{Context, Result};

use super::Protocol;
use super::pack::{Bitmap, NO_VAL, Pack, Unpack};
use super::reasons::REASONS;

/// The request for every job.
pub(crate) const REQUEST: u16 = 2003;
/// The reply to either job request.
pub(crate) const RESPONSE: u16 = 2004;
/// The request for one job, or every task of an array by its base ID.
pub(crate) const SINGLE_REQUEST: u16 = 2021;

const SHOW_ALL: u16 = 1;
const SHOW_DETAIL: u16 = 1 << 1;

/// The body of a request for every job, with each one's cores.
pub(crate) fn request() -> Vec<u8> {
    let mut body = Pack::default();
    body.time(0).u16(SHOW_ALL | SHOW_DETAIL).u32(NO_VAL);
    body.0
}

/// The body of a request for job `id`, with its cores. 25.11 names the job by a whole step ID.
pub(crate) fn single_request(id: u32, protocol: Protocol) -> Vec<u8> {
    let mut body = Pack::default();
    if protocol == Protocol::V25_11 {
        body.u64(0).u32(id).u32(NO_VAL).u32(NO_VAL);
    } else {
        body.u32(id);
    }
    body.u16(SHOW_ALL | SHOW_DETAIL);
    body.0
}

/// The tasks of a job array that one record stands for.
///
/// # Examples
///
/// ```
/// use glurmo_rpc::jobs::ArrayTasks;
///
/// let tasks = ArrayTasks { job: 3441951, spec: "0-3,8%2".into(), count: 5 };
/// assert_eq!(tasks.count, 5);
/// ```
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
///
/// # Examples
///
/// ```
/// use glurmo_rpc::jobs::Job;
///
/// let job = Job {
///     id: 4242,
///     array: None,
///     user_id: 1001,
///     user_name: Some("otter".into()),
///     partition: "gpu,5days".into(),
///     state: "PENDING",
///     reason: "Priority".into(),
///     cpus: 8,
///     tres: "cpu=8,mem=16G,node=1,gres/gpu=1".into(),
///     time_limit: 1_440,
///     submit: 1_794_000_000,
///     start: 0,
///     end: 0,
///     nodes: String::new(),
///     name: "align".into(),
/// };
/// assert_eq!(job.partition.split(',').count(), 2);
/// ```
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
    /// The time limit in minutes. [`INFINITE`](super::pack::INFINITE) is no limit, and
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
///
/// # Examples
///
/// ```
/// use glurmo_rpc::jobs::{HostCells, Placed};
///
/// let job = Placed {
///     id: 4242,
///     start: 1_794_000_100,
///     state: "RUNNING",
///     hosts: "z[1001-1002]".into(),
///     cells: vec![
///         HostCells { cores: vec![0, 1], gres: "gpu:1(IDX:2)".into(), memory_mb: 8_000 },
///         HostCells { cores: vec![4], gres: String::new(), memory_mb: 4_000 },
///     ],
/// };
/// let cores: usize = job.cells.iter().map(|cells| cells.cores.len()).sum();
/// assert_eq!(cores, 3);
/// ```
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
///
/// # Examples
///
/// ```
/// use glurmo_rpc::jobs::HostCells;
///
/// let cells = HostCells { cores: vec![0, 1], gres: "gpu:a100:2(IDX:0,2)".into(), memory_mb: 16_000 };
/// assert_eq!(cells.cores.len(), 2);
/// ```
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
    array_tasks: Option<Bitmap>,
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
    gres_detail: Vec<Option<String>>,
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
    cores: Option<Bitmap>,
}

/// Decodes a `RESPONSE_JOB_INFO` body into two views of its records. The first lists the jobs as
/// squeue does, with pending tasks that the controller split out of an array folded back into the
/// array's record. The second holds the cores of every record.
///
/// # Errors
///
/// Fails when the body does not follow `protocol`'s layout.
pub(crate) fn decode(body: &[u8], protocol: Protocol) -> Result<(Vec<Job>, Vec<Placed>)> {
    let mut records = records(body, protocol)?;
    let placed = records.iter().map(Raw::placed).collect();
    combine_pending_arrays(&mut records);
    Ok((records.iter().map(Raw::listed).collect(), placed))
}

fn records(body: &[u8], protocol: Protocol) -> Result<Vec<Raw>> {
    let u = &mut Unpack::new(body);
    let count = u.u32()?;
    let _last_update = u.time()?;
    let _last_backfill = u.time()?;
    let jobs = (0..count)
        .map(|i| {
            match protocol {
                Protocol::V23_11 => v23_11(u),
                Protocol::V25_11 => v25_11(u),
            }
            .with_context(|| format!("job {i}"))
        })
        .collect::<Result<_>>()?;
    u.finish()?;
    Ok(jobs)
}

#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field"
)]
fn v23_11(u: &mut Unpack) -> Result<Raw> {
    let array_base = u.u32()?;
    let array_task = u.u32()?;
    let array_tasks = u.str()?;
    let array_max_tasks = u.u32()?;
    let _assoc_id = u.u32()?;
    let _container = u.str()?;
    let _container_id = u.str()?;
    let _delay_boot = u.u32()?;
    let _failed_node = u.str()?;
    let id = u.u32()?;
    let user_id = u.u32()?;
    let _group_id = u.u32()?;
    let _het_job_id = u.u32()?;
    let _het_job_id_set = u.str()?;
    let _het_job_offset = u.u32()?;
    let _profile = u.u32()?;
    let state = u.u32()?;
    let _batch_flag = u.u16()?;
    let state_reason = u.u32()?;
    let _power_flags = u.u8()?;
    let _reboot = u.u8()?;
    let _restart_cnt = u.u16()?;
    let _show_flags = u.u16()?;
    let _deadline = u.time()?;
    let _alloc_sid = u.u32()?;
    let time_limit = u.u32()?;
    let _time_min = u.u32()?;
    let _nice = u.u32()?;
    let submit = u.time()?;
    let _eligible_time = u.time()?;
    let _accrue_time = u.time()?;
    let start = u.time()?;
    let end = u.time()?;
    for _time in 0..5 {
        u.time()?;
    }
    let _priority = u.u32()?;
    let _billable_tres = u.double()?;
    let _cluster = u.str()?;
    let nodes = u.str()?;
    let _sched_nodes = u.str()?;
    let partition = u.str()?;
    let _account = u.str()?;
    let _admin_comment = u.str()?;
    let _site_factor = u.u32()?;
    u.skip_strs(9)?;
    let _qos = u.str()?;
    let _preemptable_time = u.time()?;
    let _licenses = u.str()?;
    let state_desc = u.str()?;
    let _resv_name = u.str()?;
    let _mcs_label = u.str()?;
    let _exit_code = u.u32()?;
    let _derived_ec = u.u32()?;
    let _gres_total = u.str()?;
    let resources = resources(u, Protocol::V23_11)?;
    let gres_detail = u.str_array()?;
    let name = u.str()?;
    let user_name = u.str()?;
    let _wckey = u.str()?;
    let _req_switch = u.u32()?;
    let _wait4switch = u.u32()?;
    let _alloc_node = u.str()?;
    let _node_inx = u.bitmap()?;
    u.skip_strs(6)?;
    let cpus = u.u32()?;
    let _max_cpus = u.u32()?;
    let _num_nodes = u.u32()?;
    let _max_nodes = u.u32()?;
    let _job_size = u.bitmap()?;
    let _requeue = u.u16()?;
    let _ntasks_per_node = u.u16()?;
    let _ntasks_per_tres = u.u16()?;
    let _num_tasks = u.u32()?;
    let _shared = u.u16()?;
    let _cpu_freq = (u.u32()?, u.u32()?, u.u32()?);
    let _cronspec = u.str()?;
    let _contiguous = u.u16()?;
    let _core_spec = u.u16()?;
    let _cpus_per_task = u.u16()?;
    let _pn_min_cpus = u.u16()?;
    let _pn_min_memory = u.u64()?;
    let _pn_min_tmp_disk = u.u32()?;
    let _req_nodes = u.str()?;
    let _req_node_inx = u.bitmap()?;
    let _exc_nodes = u.str()?;
    let _exc_node_inx = u.bitmap()?;
    u.skip_strs(3)?;
    if u.u8()? == 0xff {
        for _field in 0..9 {
            u.u16()?;
        }
    }
    let _bitflags = u.u64()?;
    let tres_alloc = u.str()?;
    let tres_req = u.str()?;
    let _start_protocol_ver = u.u16()?;
    let _fed_origin = u.str()?;
    let _fed_siblings_active = (u.u64()?, u.str()?);
    let _fed_siblings_viable = (u.u64()?, u.str()?);
    u.skip_strs(8)?;
    let _mail_type = u.u16()?;
    let _mail_user = u.str()?;
    let _selinux_context = u.str()?;
    Ok(Raw {
        id,
        array_base,
        array_task,
        array_tasks: array_tasks.as_deref().map(hex_tasks).transpose()?,
        array_max_tasks,
        combined: false,
        user_id,
        user_name,
        state,
        state_reason,
        state_desc,
        time_limit,
        submit,
        start,
        end,
        nodes,
        partition,
        resources,
        gres_detail,
        name,
        cpus,
        tres_alloc,
        tres_req,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field"
)]
fn v25_11(u: &mut Unpack) -> Result<Raw> {
    let _sluid = u.u64()?;
    let id = u.u32()?;
    let _step = (u.u32()?, u.u32()?);
    u.skip_strs(3)?;
    let _alloc_sid = u.u32()?;
    let array_base = u.u32()?;
    let array_task = u.u32()?;
    let _assoc_id = u.u32()?;
    let _batch_features = u.str()?;
    let _batch_flag = u.u16()?;
    let _batch_host = u.str()?;
    let _bitflags = u.u64()?;
    u.skip_strs(2)?;
    let _billable_tres = u.double()?;
    u.skip_strs(4)?;
    let _deadline = u.time()?;
    let _delay_boot = u.u32()?;
    let _exit_codes = (u.u32()?, u.u32()?);
    u.skip_strs(2)?;
    if u.u8()? != 0 {
        let _fed_origin = u.str()?;
        let _fed_siblings_active = (u.u64()?, u.str()?);
        let _fed_siblings_viable = (u.u64()?, u.str()?);
    }
    let _gres_total = u.str()?;
    let _group_id = u.u32()?;
    let _het_job_id = u.u32()?;
    let _het_job_id_set = u.str()?;
    let _het_job_offset = u.u32()?;
    let state = u.u32()?;
    let _last_sched_eval = u.time()?;
    u.skip_strs(2)?;
    let _mail_type = u.u16()?;
    u.skip_strs(3)?;
    let name = u.str()?;
    let _network = u.str()?;
    let _times = (u.time()?, u.time()?);
    let _priority = u.u32()?;
    let _profile = u.u32()?;
    let _reboot = u.u8()?;
    let _req_switch = u.u32()?;
    let _resize_time = u.time()?;
    let _restart_cnt = u.u16()?;
    u.skip_strs(3)?;
    let _site_factor = u.u32()?;
    let _start_protocol_ver = u.u16()?;
    let state_desc = u.str()?;
    let state_reason = u.u32()?;
    let _suspend_time = u.time()?;
    let _system_comment = u.str()?;
    let _time_min = u.u32()?;
    let _tres_bind = u.str()?;
    let tres_alloc = u.str()?;
    let tres_req = u.str()?;
    u.skip_strs(5)?;
    let user_id = u.u32()?;
    let user_name = u.str()?;
    let _wait4switch = u.u32()?;
    let _wckey = u.str()?;
    let array_tasks = u.str()?;
    let array_max_tasks = u.u32()?;
    let time_limit = u.u32()?;
    let start = u.time()?;
    let end = u.time()?;
    let _priority_array = u.u32_array()?;
    let _priority_array_names = u.str()?;
    let _cluster = u.str()?;
    let nodes = u.str()?;
    let _sched_nodes = u.str()?;
    let partition = u.str()?;
    let _qos = u.str()?;
    let _preemptable_time = u.time()?;
    let resources = resources(u, Protocol::V25_11)?;
    let gres_detail = u.str_array()?;
    let _node_inx = u.bitmap()?;
    let (submit, cpus) = if u.u8()? == 0 {
        let cpus = u.u32()?;
        let _num_nodes_nice = (u.u32()?, u.u32()?);
        (0, cpus)
    } else {
        let _accrue_eligible = (u.time()?, u.time()?);
        let _cluster_features = u.str()?;
        let _cpu_freq = (u.u32()?, u.u32()?, u.u32()?);
        let _dependency = u.str()?;
        let _job_size = u.bitmap()?;
        let _nice = u.u32()?;
        let _ntasks_requeue_segment = (u.u16()?, u.u16()?, u.u16()?, u.u16()?);
        let submit = u.time()?;
        u.skip_strs(5)?;
        let cpus = u.u32()?;
        let _sizes = (u.u32()?, u.u32()?, u.u32()?, u.u32()?);
        let _shared = u.u16()?;
        let _cronspec = u.str()?;
        (submit, cpus)
    };
    let _contiguous_spec_cpus = (u.u16()?, u.u16()?, u.u16()?, u.u16()?);
    let _pn_min_memory = u.u64()?;
    let _pn_min_tmp_disk = u.u32()?;
    let _oom_kill_step = u.u16()?;
    let _req_nodes = (u.str()?, u.bitmap()?);
    let _exc_nodes = (u.str()?, u.bitmap()?);
    u.skip_strs(3)?;
    if u.u8()? == 0xff {
        for _field in 0..9 {
            u.u16()?;
        }
    }
    Ok(Raw {
        id,
        array_base,
        array_task,
        array_tasks: array_tasks.as_deref().map(hex_tasks).transpose()?,
        array_max_tasks,
        combined: false,
        user_id,
        user_name,
        state,
        state_reason,
        state_desc,
        time_limit,
        submit,
        start,
        end,
        nodes,
        partition,
        resources,
        gres_detail,
        name,
        cpus,
        tres_alloc,
        tres_req,
    })
}

fn resources(u: &mut Unpack, protocol: Protocol) -> Result<Option<Resources>> {
    let hosts = u.u32()?;
    if hosts == NO_VAL {
        return Ok(None);
    }
    let _ncpus = u.u32()?;
    if protocol == Protocol::V25_11 {
        let _next_step_node_inx = u.u32()?;
    }
    let _node_req = u.u32()?;
    let names = u.str()?.unwrap_or_default();
    let _whole_node = u.u8()?;
    let _threads_per_core = u.u16()?;
    let _cr_type = u.u16()?;
    let _cpu_array_reps = u.u32_array()?;
    let _cpu_array_value = u.u16_array()?;
    let _cpus = u.u16_array()?;
    let _cpus_used = u.u16_array()?;
    let memory_mb = u.u64_array()?;
    let _memory_used = u.u64_array()?;
    let sockets = u.u16_array()?;
    let cores_per_socket = u.u16_array()?;
    let repeats = u.u32_array()?;
    let cores = u.bitmap()?;
    let _cores_used = u.bitmap()?;
    if protocol == Protocol::V25_11 {
        let _node_bitmap = u.bitmap()?;
    }
    let cores_per_host = sockets
        .iter()
        .zip(&cores_per_socket)
        .zip(&repeats)
        .map(|((&sockets, &cores), &repeat)| (u32::from(sockets) * u32::from(cores), repeat))
        .collect();
    Ok(Some(Resources {
        hosts: names,
        memory_mb,
        cores_per_host,
        cores,
    }))
}

/// Reads the hex task mask the controller sends for a pending array range. Anything else, such as
/// an empty string, means no range.
pub(super) fn hex_tasks(text: &str) -> Result<Bitmap> {
    let digits = text.strip_prefix("0x").unwrap_or_default();
    let mut set = Vec::new();
    for (position, digit) in digits.bytes().rev().enumerate() {
        let nibble = char::from(digit)
            .to_digit(16)
            .with_context(|| format!("bad array task mask {text:?}"))?;
        for bit in 0..4 {
            if nibble & (1 << bit) != 0 {
                set.push(u32::try_from(position * 4 + bit)?);
            }
        }
    }
    let size = u32::try_from(text.len() * 4)?;
    Ok(Bitmap { size, set })
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
            self.array_tasks.as_ref(),
            self.array_max_tasks,
        );
        if let (true, Some(tasks), Some(range)) = (self.combined, &mut tasks, &self.array_tasks) {
            tasks.spec = ranges(&range.set);
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
        let held = self.resources.as_ref().and_then(|resources| {
            let cores = resources.cores.as_ref()?;
            Some((resources, cores, *cores.set.last()?))
        });
        let Some((resources, cores, last)) = held else {
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
                cores: cores
                    .set
                    .iter()
                    .filter(|&&core| core >= offset && core < offset + width)
                    .map(|core| core - offset)
                    .collect(),
                gres: self
                    .gres_detail
                    .get(i)
                    .cloned()
                    .flatten()
                    .unwrap_or_default(),
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
            let size = jobs[i].array_tasks.as_ref().map_or(0, |range| range.size);
            let (record, task) = (&jobs[i], &jobs[j]);
            if i == j
                || folded[j]
                || !pending(task)
                || task.array_base != record.array_base
                || task.array_task >= size
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
            if let Some(range) = &mut record.array_tasks
                && let Err(at) = range.set.binary_search(&task_id)
            {
                range.set.insert(at, task_id);
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
    range: Option<&Bitmap>,
    max_tasks: u32,
) -> Option<ArrayTasks> {
    match range {
        Some(range) if !range.set.is_empty() => Some(ArrayTasks {
            job,
            spec: task_spec(&range.set, max_tasks),
            count: u32::try_from(range.set.len()).unwrap_or(u32::MAX),
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
    match REASONS.get(reason as usize) {
        Some(name) if !name.is_empty() => (*name).to_owned(),
        _ => reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(id: u32, task: u32, tasks: Option<&str>, name: &str, reason: u32) -> Raw {
        Raw {
            id,
            array_base: 10,
            array_task: task,
            array_tasks: tasks.map(|hex| hex_tasks(hex).unwrap()),
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
            cores: Some(Bitmap {
                size: 8,
                set: vec![0, 1, 6],
            }),
        });
        job.gres_detail = vec![Some("gpu:1(IDX:0)".into()), None];
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
            pending(10, NO_VAL, Some("0x0C"), "n", 1),
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
    fn hex_task_masks_count_from_the_last_digit() {
        assert_eq!(hex_tasks("0x0F").unwrap().set, [0, 1, 2, 3]);
        assert_eq!(hex_tasks("").unwrap().set, Vec::<u32>::new());
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
                cores: Some(Bitmap {
                    size: 12,
                    set: vec![0, 1, 4, 5, 6, 11],
                }),
            }),
            gres_detail: vec![Some("gpu:1(IDX:2)".into()), None],
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
