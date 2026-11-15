//! Jobs and their placement, from `RESPONSE_JOB_INFO`.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::pack::{Bitmap, INFINITE, NO_VAL, Pack, Unpack};
use super::reasons::REASONS;
use crate::slurm::fields::{gpu_indices, gres, job_state, tres};
use crate::slurm::hostlist::expand_hosts;
use crate::slurm::scontrol::Placed;
use glurmo_model::{
    ArrayTasks, Attempt, AttemptKey, JobId, Login, NodeCells, PartitionName, Placement, UnixTime,
};

/// The request for every job.
pub const REQUEST: u16 = 2003;
/// The request for one job, or every task of an array by its base ID.
pub const SINGLE_REQUEST: u16 = 2021;

const SHOW_ALL: u16 = 1;
const SHOW_DETAIL: u16 = 1 << 1;

/// The body of a request for every job, with each one's cores.
pub fn request() -> Vec<u8> {
    let mut body = Pack::default();
    body.time(0).u16(SHOW_ALL | SHOW_DETAIL).u32(NO_VAL);
    body.0
}

/// The body of a request for job `id`, with its cores.
pub fn single_request(id: JobId) -> Vec<u8> {
    let mut body = Pack::default();
    body.u32(id.0).u16(SHOW_ALL | SHOW_DETAIL);
    body.0
}

/// One job as the controller holds it, reduced to what glurmo reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    id: JobId,
    array_base: u32,
    array_task: u32,
    array_tasks: Option<Bitmap>,
    array_max_tasks: u32,
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

/// Decodes a `RESPONSE_JOB_INFO` body.
///
/// # Errors
///
/// Fails when the body does not follow Slurm 23.11's layout.
pub fn decode(body: &[u8]) -> Result<Vec<Job>> {
    let u = &mut Unpack::new(body);
    let count = u.u32()?;
    let _last_update = u.time()?;
    let _last_backfill = u.time()?;
    (0..count)
        .map(|i| job(u).with_context(|| format!("job {i}")))
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field"
)]
fn job(u: &mut Unpack) -> Result<Job> {
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
    let resources = resources(u)?;
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
    Ok(Job {
        id: JobId(id),
        array_base,
        array_task,
        array_tasks: array_tasks.as_deref().map(hex_tasks).transpose()?,
        array_max_tasks,
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

fn resources(u: &mut Unpack) -> Result<Option<Resources>> {
    let hosts = u.u32()?;
    if hosts == NO_VAL {
        return Ok(None);
    }
    let _ncpus = u.u32()?;
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
    let size = u32::try_from(digits.len() * 4)?;
    Ok(Bitmap { size, set })
}

impl Job {
    /// The UID of the job's owner.
    pub fn user_id(&self) -> u32 {
        self.user_id
    }

    /// The job as squeue lists it. `logins` names its owner, as squeue does from the local account
    /// database, because the controller's own name for a UID can differ. `now` decides whether an
    /// unlimited job's end is only a guess.
    ///
    /// # Errors
    ///
    /// Fails when its node list, TRES or state does not parse, or it has no submit time.
    pub fn attempt(&self, now: UnixTime, logins: &HashMap<u32, Login>) -> Result<Attempt> {
        let state_raw = state_name(self.state).to_owned();
        let tres = tres(
            self.tres_alloc
                .as_deref()
                .or(self.tres_req.as_deref())
                .unwrap_or_default(),
        )?;
        let reason = match &self.state_desc {
            Some(desc) => desc.clone(),
            None => reason_name(self.state_reason),
        };
        let unlimited_end = self.time_limit == INFINITE && self.end > now.0;
        Ok(Attempt {
            key: AttemptKey {
                job: self.id,
                submit: epoch(self.submit).context("no submit time")?,
            },
            array: self.array(),
            login: logins.get(&self.user_id).cloned().unwrap_or_else(|| {
                Login(
                    self.user_name
                        .clone()
                        .unwrap_or_else(|| self.user_id.to_string()),
                )
            }),
            partitions: self
                .partition
                .as_deref()
                .unwrap_or_default()
                .split(',')
                .map(|name| PartitionName(name.to_owned()))
                .collect(),
            state: job_state(&state_raw)?,
            state_raw,
            cpus: self.cpus,
            mem_mb: tres.mem_mb,
            gpus: tres.gpus,
            time_limit_s: (self.time_limit != INFINITE && self.time_limit != NO_VAL)
                .then(|| u64::from(self.time_limit) * 60),
            start: epoch(self.start),
            end: if unlimited_end { None } else { epoch(self.end) },
            nodes: expand_hosts(self.nodes.as_deref().unwrap_or_default())?,
            reason: (reason != "None" && !reason.is_empty()).then_some(reason),
            name: self.name.clone().unwrap_or_default(),
        })
    }

    fn array(&self) -> Option<ArrayTasks> {
        array_tasks(
            self.array_base,
            self.array_task,
            self.array_tasks.as_ref(),
            self.array_max_tasks,
        )
    }

    /// The job's cores, GPUs and memory on each node, as `scontrol -d` reports them.
    ///
    /// # Errors
    ///
    /// Fails when its node list, GRES or state does not parse.
    pub fn placed(&self) -> Result<Placed> {
        Ok(Placed {
            job: self.id,
            start: epoch(self.start),
            state: job_state(state_name(self.state))?,
            placement: self.placement()?,
        })
    }

    fn placement(&self) -> Result<Placement> {
        let Some(resources) = &self.resources else {
            return Ok(Placement(Vec::new()));
        };
        let Some(cores) = resources
            .cores
            .as_ref()
            .filter(|cores| !cores.set.is_empty())
        else {
            return Ok(Placement(Vec::new()));
        };
        let last = *cores.set.last().expect("not empty");
        let hosts = expand_hosts(&resources.hosts)?;
        let widths = resources
            .cores_per_host
            .iter()
            .flat_map(|&(width, repeat)| std::iter::repeat_n(width, repeat as usize));
        let mut cells = Vec::new();
        let mut offset = 0;
        for (i, (host, width)) in hosts.into_iter().zip(widths).enumerate() {
            let held = cores
                .set
                .iter()
                .filter(|&&core| core >= offset && core < offset + width)
                .map(|core| core - offset)
                .collect();
            let detail = self
                .gres_detail
                .get(i)
                .cloned()
                .flatten()
                .unwrap_or_default();
            cells.push(NodeCells {
                node: host,
                cores: held,
                unplaced: 0,
                gpus: gpu_indices(&gres(&detail)?)?,
                mem_mb: Some(resources.memory_mb.get(i).copied().unwrap_or(0)),
            });
            offset += width;
            if offset > last {
                break;
            }
        }
        Ok(Placement(cells))
    }
}

/// The array tasks a record stands for: a pending range, one task, or none.
pub(super) fn array_tasks(
    base: u32,
    task: u32,
    range: Option<&Bitmap>,
    max_tasks: u32,
) -> Option<ArrayTasks> {
    let job = JobId(base);
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

pub(super) fn epoch(time: i64) -> Option<UnixTime> {
    (time != 0).then_some(UnixTime(time))
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

/// `job_state_string`: a flag wins over the base state.
pub(super) fn state_name(state: u32) -> &'static str {
    const FLAGS: [(u32, &str); 12] = [
        (1 << 15, "COMPLETING"),
        (1 << 23, "STAGE_OUT"),
        (1 << 14, "CONFIGURING"),
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
        || BASES.get((state & 0xff) as usize).copied().unwrap_or("?"),
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
}
