//! Nodes and partitions, from `RESPONSE_NODE_INFO` and `RESPONSE_PARTITION_INFO`.

use anyhow::{Context, Result, bail};

use super::pack::{INFINITE, NO_VAL, NO_VAL64, Pack, Unpack};
use crate::slurm::fields::{gpu_count, gpu_model, gres, node_state};
use crate::slurm::hostlist::expand_hosts;
use glurmo_model::{Node, NodeName, Partition, PartitionName};

/// The node request.
pub const NODE_REQUEST: u16 = 2007;
/// The partition request.
pub const PARTITION_REQUEST: u16 = 2009;

const SHOW_ALL: u16 = 1;
const SHOW_DETAIL: u16 = 1 << 1;

/// The body of the partition request: every partition, hidden ones included.
pub fn partition_request() -> Vec<u8> {
    let mut body = Pack::default();
    body.time(0).u16(SHOW_ALL);
    body.0
}

/// The body of the node request. The controller fills in GRES used only with `SHOW_DETAIL`.
pub fn node_request() -> Vec<u8> {
    let mut body = Pack::default();
    body.time(0).u16(SHOW_ALL | SHOW_DETAIL);
    body.0
}

/// A partition and the indices of its nodes in the node reply.
pub struct PartitionRecord {
    /// The partition.
    pub partition: Partition,
    /// Its nodes, as positions in the node reply.
    pub node_indices: Vec<u32>,
}

/// Decodes a `RESPONSE_PARTITION_INFO` body.
///
/// # Errors
///
/// Fails when the body does not follow Slurm 23.11's layout.
pub fn partitions(body: &[u8]) -> Result<Vec<PartitionRecord>> {
    let u = &mut Unpack::new(body);
    let count = u.u32()?;
    let _last_update = u.time()?;
    (0..count)
        .map(|i| partition(u).with_context(|| format!("partition {i}")))
        .collect()
}

fn partition(u: &mut Unpack) -> Result<PartitionRecord> {
    const DEFAULT: u16 = 1;
    const HIDDEN: u16 = 1 << 1;
    let name = u.str()?.unwrap_or_default();
    let _cpu_bind = u.u32()?;
    let _grace_time = u.u32()?;
    let max_time = u.u32()?;
    let _default_time = u.u32()?;
    let _max_nodes = u.u32()?;
    let _min_nodes = u.u32()?;
    let _total_nodes = u.u32()?;
    let _total_cpus = u.u32()?;
    let _def_mem_per_cpu = u.u64()?;
    let _max_cpus_per_node = u.u32()?;
    let _max_cpus_per_socket = u.u32()?;
    let _max_mem_per_cpu = u.u64()?;
    let flags = u.u16()?;
    let _max_share = u.u16()?;
    let _over_time_limit = u.u16()?;
    let _preempt_mode = u.u16()?;
    let _priority_job_factor = u.u16()?;
    let _priority_tier = u.u16()?;
    let state = u.u16()?;
    let _cr_type = u.u16()?;
    let _resume_timeout = u.u16()?;
    let _suspend_timeout = u.u16()?;
    let _suspend_time = u.u32()?;
    u.skip_strs(8)?;
    let nodes = u.str()?.unwrap_or_default();
    let _nodesets = u.str()?;
    let node_indices = u.bitmap()?.map_or_default(|bitmap| bitmap.set);
    let _billing_weights = u.str()?;
    let _tres = u.str()?;
    u.list(|u| {
        let _type = u.u16()?;
        u.u64()
    })?;
    Ok(PartitionRecord {
        partition: Partition {
            name: PartitionName(name),
            nodes: expand_hosts(&nodes)?,
            max_time_s: (max_time != INFINITE && max_time != NO_VAL)
                .then(|| u64::from(max_time) * 60),
            default: flags & DEFAULT != 0,
            hidden: flags & HIDDEN != 0,
            state_raw: partition_state(state).to_owned(),
        },
        node_indices,
    })
}

/// Names a partition state as scontrol does.
fn partition_state(state: u16) -> &'static str {
    match state {
        3 => "UP",
        1 => "DOWN",
        2 => "DRAIN",
        0 => "INACTIVE",
        _ => "UNKNOWN",
    }
}

/// Decodes a `RESPONSE_NODE_INFO` body into one node per record, with each node's partitions in
/// configuration order, as `sinfo -N -S N,#P` lists them. Like sinfo, it leaves out nodes in no partition.
///
/// # Errors
///
/// Fails when the body does not follow Slurm 23.11's layout or a GRES string does not parse.
pub fn nodes(body: &[u8], partitions: &[PartitionRecord]) -> Result<Vec<Node>> {
    let u = &mut Unpack::new(body);
    let count = u.u32()?;
    let _last_update = u.time()?;
    let mut nodes = (0..count)
        .map(|i| node(u).with_context(|| format!("node {i}")))
        .collect::<Result<Vec<_>>>()?;
    for record in partitions {
        for &i in &record.node_indices {
            let node = nodes
                .get_mut(i as usize)
                .with_context(|| format!("partition node index {i} past {count} nodes"))?;
            node.partitions.push(record.partition.name.clone());
        }
    }
    nodes.retain(|node| !node.partitions.is_empty());
    Ok(nodes)
}

fn node(u: &mut Unpack) -> Result<Node> {
    let name = u.str()?.context("a node without a name")?;
    u.skip_strs(3)?;
    let _port = u.u16()?;
    let _next_state = u.u32()?;
    let state = u.u32()?;
    let _version = u.str()?;
    let cpus = u.u16()?;
    let _boards = u.u16()?;
    let sockets = u.u16()?;
    let cores_per_socket = u.u16()?;
    let _threads = u.u16()?;
    let real_memory = u.u64()?;
    let _tmp_disk = u.u32()?;
    let _mcs_label = u.str()?;
    let _owner = u.u32()?;
    let _core_spec_cnt = u.u16()?;
    let _cpu_bind = u.u32()?;
    let _mem_spec_limit = u.u64()?;
    let _cpu_spec_list = u.str()?;
    let cpus_effective = u.u16()?;
    let cpu_load = u.u32()?;
    let free_mem = u.u64()?;
    let _weight = u.u32()?;
    let _reason_uid = u.u32()?;
    for _time in 0..5 {
        u.time()?;
    }
    let plugin = u.u32()?;
    if !matches!(plugin, CONS_TRES | LINEAR) {
        bail!("select plugin {plugin} is not cons_tres or linear");
    }
    let alloc_cpus = u.u16()?;
    let alloc_memory = u.u64()?;
    let alloc_tres = u.str()?;
    let _alloc_tres_weighted = u.double()?;
    let _arch = u.str()?;
    let features = u.str()?;
    let _features_active = u.str()?;
    let gres_total = u.str()?.unwrap_or_default();
    let _gres_drain = u.str()?;
    let gres_used = u.str()?.unwrap_or_default();
    u.skip_strs(5)?;
    let reason = u.str()?;
    let _energy = (u.u64()?, u.u32()?, u.u64()?, u.u32()?, u.u64()?, u.time()?);
    let _ext_sensors = (u.u64()?, u.u32()?, u.time()?, u.u32()?);
    let _power_cap = u.u32()?;
    let _tres = u.str()?;
    let _reservation = u.str()?;

    let state = mixed(state, cpus_effective, alloc_cpus, alloc_tres.as_deref());
    let state_raw = state_complete(state);
    let (state, flags) = node_state(&state_raw);
    let gres_total = gres(&gres_total)?;
    Ok(Node {
        name: NodeName(name),
        partitions: Vec::new(),
        state,
        flags,
        state_raw,
        reason: reason.filter(|reason| !reason.is_empty()),
        cpus: u32::from(cpus),
        cpus_alloc: u32::from(alloc_cpus),
        sockets: u32::from(sockets),
        cores: u32::from(sockets) * u32::from(cores_per_socket),
        load: (cpu_load != NO_VAL).then(|| f64::from(cpu_load) / 100.0),
        mem_mb: real_memory,
        mem_alloc_mb: alloc_memory,
        mem_free_mb: (free_mem != NO_VAL64).then_some(free_mem),
        gpus: gpu_count(&gres_total),
        gpus_used: gpu_count(&gres(&gres_used)?),
        gpu_model: gpu_model(&gres_total),
        features: features
            .filter(|features| !features.is_empty())
            .map_or_default(|features| features.split(',').map(str::to_owned).collect()),
    })
}

const CONS_TRES: u32 = 109;
const LINEAR: u32 = 102;
const STATE_BASE: u32 = 0xf;
const MIXED: u32 = 5;

/// Marks a partly allocated node MIXED, as libslurm does for `SHOW_MIXED`.
fn mixed(state: u32, cpus_effective: u16, alloc_cpus: u16, alloc_tres: Option<&str>) -> u32 {
    let idle = cpus_effective.wrapping_sub(alloc_cpus);
    let partly = idle != 0 && idle < cpus_effective;
    let only_other_tres = alloc_tres.is_some() && idle == cpus_effective;
    if partly || only_other_tres {
        state & !STATE_BASE | MIXED
    } else {
        state
    }
}

/// Spells a node state as sinfo's `StateComplete` does: the base state, then each flag, in lowercase.
fn state_complete(state: u32) -> String {
    const BASES: [&str; 7] = [
        "unknown",
        "down",
        "idle",
        "allocated",
        "error",
        "mixed",
        "future",
    ];
    const FLAGS: [(u32, &str); 20] = [
        (1 << 7, "cloud"),
        (1 << 10, "completing"),
        (1 << 9, "drain"),
        (1 << 19, "dynamic_future"),
        (1 << 26, "dynamic_norm"),
        (1 << 22, "invalid_reg"),
        (1 << 13, "fail"),
        (1 << 15, "maintenance"),
        (1 << 23, "power_down"),
        (1 << 24, "power_up"),
        (1 << 4, "perfctrs"),
        (1 << 12, "powered_down"),
        (1 << 16, "reboot_requested"),
        (1 << 20, "reboot_issued"),
        (1 << 5, "reserved"),
        (1 << 8, "resume"),
        (1 << 11, "not_responding"),
        (1 << 21, "planned"),
        (1 << 14, "powering_up"),
        (1 << 18, "powering_down"),
    ];
    let mut text = BASES
        .get((state & STATE_BASE) as usize)
        .copied()
        .unwrap_or("invalid")
        .to_owned();
    let mut flags = state & !STATE_BASE;
    while flags != 0 {
        let flag = FLAGS
            .iter()
            .find(|(bit, _)| flags & bit != 0)
            .map_or((flags.isolate_lowest_one(), "?"), |&flag| flag);
        flags &= !flag.0;
        text.push('+');
        text.push_str(flag.1);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_spell_like_sinfo() {
        assert_eq!(
            state_complete(2 | 1 << 9 | 1 << 22),
            "idle+drain+invalid_reg"
        );
        assert_eq!(state_complete(1 | 1 << 11), "down+not_responding");
        assert_eq!(state_complete(2 | 1 << 10), "idle+completing");
        assert_eq!(state_complete(3 | 1 << 31), "allocated+?");
    }

    #[test]
    fn partly_allocated_nodes_are_mixed() {
        assert_eq!(mixed(3, 256, 32, Some("cpu=32")), 5);
        assert_eq!(mixed(3, 72, 72, Some("cpu=72")), 3);
        assert_eq!(mixed(2, 88, 0, None), 2);
        assert_eq!(mixed(2 | 1 << 9, 8, 0, Some("gres/gpu=1")), 5 | 1 << 9);
    }
}
