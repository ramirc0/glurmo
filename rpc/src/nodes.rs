//! Nodes and partitions, from `RESPONSE_NODE_INFO` and `RESPONSE_PARTITION_INFO`.

use anyhow::{Context, Result, bail};

use super::Protocol;
use super::pack::{INFINITE, NO_VAL, NO_VAL64, Pack, Unpack};

/// The node request.
pub(crate) const NODE_REQUEST: u16 = 2007;
/// The node reply.
pub(crate) const NODE_RESPONSE: u16 = 2008;
/// The partition request.
pub(crate) const PARTITION_REQUEST: u16 = 2009;
/// The partition reply.
pub(crate) const PARTITION_RESPONSE: u16 = 2010;

const SHOW_ALL: u16 = 1;
const SHOW_DETAIL: u16 = 1 << 1;

/// The body of the partition request: every partition, hidden ones included.
pub(crate) fn partition_request() -> Vec<u8> {
    let mut body = Pack::default();
    body.time(0).u16(SHOW_ALL);
    body.0
}

/// The body of the node request. The controller fills in GRES used only with `SHOW_DETAIL`.
pub(crate) fn node_request() -> Vec<u8> {
    let mut body = Pack::default();
    body.time(0).u16(SHOW_ALL | SHOW_DETAIL);
    body.0
}

/// A partition as scontrol shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partition {
    /// The name.
    pub name: String,
    /// Its nodes, as a hostlist such as `z[1001-1004]`.
    pub nodes: String,
    /// The longest time limit a job may ask for, or `None` for no limit.
    pub max_time_s: Option<u64>,
    /// Whether jobs that name no partition land here.
    pub default: bool,
    /// Whether the controller hides it from plain listings.
    pub hidden: bool,
    /// The state as scontrol prints it, such as `UP`.
    pub state: &'static str,
}

/// A partition and the indices of its nodes in the node reply.
pub(crate) struct PartitionRecord {
    /// The partition.
    pub(crate) partition: Partition,
    /// Its nodes, as positions in the node reply.
    pub(crate) node_indices: Vec<u32>,
}

/// A node as sinfo lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// The hostname.
    pub name: String,
    /// The partitions that contain it, in configuration order.
    pub partitions: Vec<String>,
    /// The state as sinfo's `StateComplete` spells it, such as `mixed+drain`. A partly allocated node
    /// reads as mixed.
    pub state: String,
    /// Why an operator drained it or took it down.
    pub reason: Option<String>,
    /// CPUs, which count hardware threads when the node schedules them.
    pub cpus: u32,
    /// CPUs allocated to jobs.
    pub cpus_alloc: u32,
    /// Sockets.
    pub sockets: u32,
    /// Cores across all sockets.
    pub cores: u32,
    /// The load average, if the node has reported one.
    pub load: Option<f64>,
    /// Memory, in MB.
    pub mem_mb: u64,
    /// Memory allocated to jobs, in MB.
    pub mem_alloc_mb: u64,
    /// Memory free in the OS, in MB, if the node has reported it.
    pub mem_free_mb: Option<u64>,
    /// Its GRES, as a list such as `gpu:a100:4`.
    pub gres: String,
    /// The GRES jobs hold, as a list such as `gpu:a100:2(IDX:0,2)`.
    pub gres_used: String,
    /// Feature tags.
    pub features: Vec<String>,
}

/// Decodes a `RESPONSE_PARTITION_INFO` body.
///
/// # Errors
///
/// Fails when the body does not follow `protocol`'s layout.
pub(crate) fn partitions(body: &[u8], protocol: Protocol) -> Result<Vec<PartitionRecord>> {
    let u = &mut Unpack::new(body);
    let count = u.u32()?;
    let _last_update = u.time()?;
    let partitions = (0..count)
        .map(|i| partition(u, protocol).with_context(|| format!("partition {i}")))
        .collect::<Result<_>>()?;
    u.finish()?;
    Ok(partitions)
}

fn partition(u: &mut Unpack, protocol: Protocol) -> Result<PartitionRecord> {
    const DEFAULT: u32 = 1;
    const HIDDEN: u32 = 1 << 1;
    let v25_11 = protocol == Protocol::V25_11;
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
    let flags = if v25_11 {
        u.u32()?
    } else {
        u32::from(u.u16()?)
    };
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
    if v25_11 {
        let _topology_name = u.str()?;
    }
    let _tres = u.str()?;
    u.list(|u| {
        let _type = u.u16()?;
        u.u64()
    })?;
    Ok(PartitionRecord {
        partition: Partition {
            name,
            nodes,
            max_time_s: (max_time != INFINITE && max_time != NO_VAL)
                .then(|| u64::from(max_time) * 60),
            default: flags & DEFAULT != 0,
            hidden: flags & HIDDEN != 0,
            state: partition_state(state),
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
/// Fails when the body does not follow `protocol`'s layout.
pub(crate) fn nodes(
    body: &[u8],
    partitions: &[PartitionRecord],
    protocol: Protocol,
) -> Result<Vec<Node>> {
    let u = &mut Unpack::new(body);
    let count = u.u32()?;
    let _last_update = u.time()?;
    let hidden = match protocol {
        Protocol::V23_11 => Vec::new(),
        Protocol::V25_11 => u.bitmap()?.map_or_default(|bitmap| bitmap.set),
    };
    let mut nodes = (0..count)
        .map(|i| {
            if hidden.contains(&i) {
                return Ok(None);
            }
            let raw = match protocol {
                Protocol::V23_11 => v23_11(u),
                Protocol::V25_11 => v25_11(u),
            };
            raw.map(|raw| raw.node(protocol))
                .with_context(|| format!("node {i}"))
        })
        .collect::<Result<Vec<_>>>()?;
    u.finish()?;
    for record in partitions {
        for &i in &record.node_indices {
            let slot = nodes
                .get_mut(i as usize)
                .with_context(|| format!("partition node index {i} past {count} nodes"))?;
            if let Some(node) = slot {
                node.partitions.push(record.partition.name.clone());
            }
        }
    }
    Ok(nodes
        .into_iter()
        .flatten()
        .filter(|node| !node.partitions.is_empty())
        .collect())
}

/// The fields glurmo reads from one node record.
struct Raw {
    /// `None` in the blank record a controller below 24.11 packs for a hidden node or an empty slot.
    name: Option<String>,
    state: u32,
    cpus: u16,
    sockets: u16,
    cores_per_socket: u16,
    real_memory: u64,
    cpus_effective: u16,
    cpu_load: u32,
    free_mem: u64,
    alloc_cpus: u16,
    alloc_memory: u64,
    alloc_tres: Option<String>,
    features: Option<String>,
    gres_total: String,
    gres_used: String,
    reason: Option<String>,
}

fn v23_11(u: &mut Unpack) -> Result<Raw> {
    let name = u.str()?;
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
    Ok(Raw {
        name,
        state,
        cpus,
        sockets,
        cores_per_socket,
        real_memory,
        cpus_effective,
        cpu_load,
        free_mem,
        alloc_cpus,
        alloc_memory,
        alloc_tres,
        features,
        gres_total,
        gres_used,
        reason,
    })
}

fn v25_11(u: &mut Unpack) -> Result<Raw> {
    let name = Some(u.str()?.context("a node without a name")?);
    u.skip_strs(3)?;
    let _cert_flags = u.u16()?;
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
    let _gpu_spec = u.str()?;
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
    let _res_cores_per_gpu = u.u16()?;
    let _reason_uid = u.u32()?;
    for _time in 0..6 {
        u.time()?;
    }
    let alloc_cpus = u.u16()?;
    let alloc_memory = u.u64()?;
    let alloc_tres = u.str()?;
    let _arch = u.str()?;
    let features = u.str()?;
    let _features_active = u.str()?;
    let gres_total = u.str()?.unwrap_or_default();
    let _gres_drain = u.str()?;
    let gres_used = u.str()?.unwrap_or_default();
    u.skip_strs(6)?;
    let reason = u.str()?;
    let _energy = (
        u.u64()?,
        u.u32()?,
        u.u64()?,
        u.u32()?,
        u.u64()?,
        u.time()?,
        u.time()?,
    );
    let _tres = u.str()?;
    let _reservation = u.str()?;
    let _topology = u.str()?;
    Ok(Raw {
        name,
        state,
        cpus,
        sockets,
        cores_per_socket,
        real_memory,
        cpus_effective,
        cpu_load,
        free_mem,
        alloc_cpus,
        alloc_memory,
        alloc_tres,
        features,
        gres_total,
        gres_used,
        reason,
    })
}

impl Raw {
    /// The node, or `None` for a blank record, which sinfo skips too.
    fn node(self, protocol: Protocol) -> Option<Node> {
        let state = mixed(
            self.state,
            self.cpus_effective,
            self.alloc_cpus,
            self.alloc_tres.as_deref(),
        );
        Some(Node {
            name: self.name?,
            partitions: Vec::new(),
            state: state_complete(state, protocol),
            reason: self.reason.filter(|reason| !reason.is_empty()),
            cpus: u32::from(self.cpus),
            cpus_alloc: u32::from(self.alloc_cpus),
            sockets: u32::from(self.sockets),
            cores: u32::from(self.sockets) * u32::from(self.cores_per_socket),
            load: (self.cpu_load != NO_VAL).then(|| f64::from(self.cpu_load) / 100.0),
            mem_mb: self.real_memory,
            mem_alloc_mb: self.alloc_memory,
            mem_free_mb: (self.free_mem != NO_VAL64).then_some(self.free_mem),
            gres: self.gres_total,
            gres_used: self.gres_used,
            features: self
                .features
                .filter(|features| !features.is_empty())
                .map_or_default(|features| features.split(',').map(str::to_owned).collect()),
        })
    }
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
fn state_complete(state: u32, protocol: Protocol) -> String {
    const BASES: [&str; 7] = [
        "unknown",
        "down",
        "idle",
        "allocated",
        "error",
        "mixed",
        "future",
    ];
    const FLAGS_23_11: [(u32, &str); 20] = [
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
    const FLAGS_25_11: [(u32, &str); 21] = [
        (1 << 27, "blocked"),
        (1 << 7, "cloud"),
        (1 << 10, "completing"),
        (1 << 9, "drain"),
        (1 << 19, "dynamic_future"),
        (1 << 26, "dynamic_norm"),
        (1 << 4, "external"),
        (1 << 22, "invalid_reg"),
        (1 << 13, "fail"),
        (1 << 15, "maintenance"),
        (1 << 23, "power_down"),
        (1 << 24, "power_up"),
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
    let flags_in_order: &[(u32, &str)] = match protocol {
        Protocol::V23_11 => &FLAGS_23_11,
        Protocol::V25_11 => &FLAGS_25_11,
    };
    let mut text = BASES
        .get((state & STATE_BASE) as usize)
        .copied()
        .unwrap_or("invalid")
        .to_owned();
    let mut flags = state & !STATE_BASE;
    while flags != 0 {
        let flag = flags_in_order
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
            state_complete(2 | 1 << 9 | 1 << 22, Protocol::V23_11),
            "idle+drain+invalid_reg"
        );
        assert_eq!(
            state_complete(1 | 1 << 11, Protocol::V23_11),
            "down+not_responding"
        );
        assert_eq!(
            state_complete(2 | 1 << 10, Protocol::V23_11),
            "idle+completing"
        );
        assert_eq!(
            state_complete(2 | 1 << 4, Protocol::V23_11),
            "idle+perfctrs"
        );
        assert_eq!(
            state_complete(2 | 1 << 4 | 1 << 27, Protocol::V25_11),
            "idle+blocked+external"
        );
        assert_eq!(state_complete(3 | 1 << 31, Protocol::V23_11), "allocated+?");
    }

    /// A 23.11 node record named `name`, idle, with 8 CPUs. `None` packs the blank record the
    /// controller sends for a FUTURE node or an empty slot.
    fn v23_11_record(body: &mut Pack, name: Option<&str>) {
        body.str(name).str(None).str(None).str(None).u16(0).u32(0);
        body.u32(if name.is_some() { 2 } else { 0 }).str(None);
        body.u16(8).u16(1).u16(1).u16(8).u16(1).u64(1024).u32(0);
        body.str(None).u32(0).u16(0).u32(0).u64(0).str(None).u16(8);
        body.u32(0).u64(0).u32(1).u32(0);
        for _time in 0..5 {
            body.time(0);
        }
        body.u32(CONS_TRES).u16(0).u64(0).str(None).u64(0);
        for _str in 0..12 {
            body.str(None);
        }
        body.u64(0).u32(0).u64(0).u32(0).u64(0).time(0);
        body.u64(0).u32(0).time(0).u32(0);
        body.u32(0).str(None).str(None);
    }

    #[test]
    fn nameless_records_below_24_11_are_skipped() {
        let mut body = Pack::default();
        body.u32(3).time(0);
        v23_11_record(&mut body, None);
        v23_11_record(&mut body, Some("z1"));
        v23_11_record(&mut body, None);
        let partition = PartitionRecord {
            partition: Partition {
                name: "short".into(),
                nodes: "z1".into(),
                max_time_s: None,
                default: true,
                hidden: false,
                state: "UP",
            },
            node_indices: vec![1],
        };
        let found = nodes(&body.0, &[partition], Protocol::V23_11).unwrap();
        let names: Vec<&str> = found.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, ["z1"]);
        assert_eq!(found[0].state, "idle");
    }

    #[test]
    fn partly_allocated_nodes_are_mixed() {
        assert_eq!(mixed(3, 256, 32, Some("cpu=32")), 5);
        assert_eq!(mixed(3, 72, 72, Some("cpu=72")), 3);
        assert_eq!(mixed(2, 88, 0, None), 2);
        assert_eq!(mixed(2 | 1 << 9, 8, 0, Some("gres/gpu=1")), 5 | 1 << 9);
    }

    /// A draining, reserved node with a distinct value in each field glurmo reads.
    fn raw() -> Raw {
        Raw {
            name: Some("z1".into()),
            state: 3 | 1 << 9 | 1 << 5,
            cpus: 16,
            sockets: 2,
            cores_per_socket: 4,
            real_memory: 64_000,
            cpus_effective: 16,
            cpu_load: 250,
            free_mem: 30_000,
            alloc_cpus: 16,
            alloc_memory: 48_000,
            alloc_tres: Some("cpu=16".into()),
            features: Some("skylake,avx512".into()),
            gres_total: "gpu:v100:4".into(),
            gres_used: "gpu:v100:1(IDX:0)".into(),
            reason: Some("bad fan".into()),
        }
    }

    #[test]
    fn every_node_value_lands_in_its_own_field() {
        let node = raw().node(Protocol::V23_11).unwrap();
        let expected = Node {
            name: "z1".into(),
            partitions: Vec::new(),
            state: "allocated+drain+reserved".into(),
            reason: Some("bad fan".into()),
            cpus: 16,
            cpus_alloc: 16,
            sockets: 2,
            cores: 8,
            load: Some(2.5),
            mem_mb: 64_000,
            mem_alloc_mb: 48_000,
            mem_free_mb: Some(30_000),
            gres: "gpu:v100:4".into(),
            gres_used: "gpu:v100:1(IDX:0)".into(),
            features: vec!["skylake".into(), "avx512".into()],
        };
        assert_eq!(node, expected);
    }

    #[test]
    fn values_the_controller_lacks_are_none() {
        let unknown = Raw {
            cpu_load: NO_VAL,
            free_mem: NO_VAL64,
            reason: Some(String::new()),
            features: None,
            ..raw()
        };
        let node = unknown.node(Protocol::V23_11).unwrap();
        assert_eq!((node.load, node.mem_free_mb), (None, None));
        assert_eq!(node.reason, None);
        assert_eq!(node.features, Vec::<String>::new());
    }

    /// A reply holding one partition record. `flags` is `DEFAULT` (1) or `HIDDEN` (2).
    fn partition_record(
        body: &mut Pack,
        protocol: Protocol,
        name: &str,
        max_time: u32,
        flags: u16,
    ) {
        body.str(Some(name)).u32(0).u32(0).u32(max_time);
        body.u32(0).u32(0).u32(0).u32(0).u32(0);
        body.u64(0).u32(0).u32(0).u64(0);
        if protocol == Protocol::V25_11 {
            body.u32(u32::from(flags));
        } else {
            body.u16(flags);
        }
        body.u16(0).u16(0).u16(0).u16(0).u16(0);
        body.u16(if flags == 2 { 2 } else { 3 });
        body.u16(0).u16(0).u16(0).u32(0);
        for _str in 0..8 {
            body.str(None);
        }
        body.str(Some("z[1-2]")).str(None);
        body.u32(4).str(Some("0x6")).str(None);
        if protocol == Protocol::V25_11 {
            body.str(None);
        }
        body.str(None).u32(0);
    }

    #[test]
    fn every_partition_value_lands_in_its_own_field() {
        for protocol in Protocol::NEWEST_FIRST {
            let mut body = Pack::default();
            body.u32(2).time(0);
            partition_record(&mut body, protocol, "gpu", 120, 2);
            partition_record(&mut body, protocol, "all", INFINITE, 1);
            let records = partitions(&body.0, protocol).unwrap();
            let expected = [
                Partition {
                    name: "gpu".into(),
                    nodes: "z[1-2]".into(),
                    max_time_s: Some(7_200),
                    default: false,
                    hidden: true,
                    state: "DRAIN",
                },
                Partition {
                    name: "all".into(),
                    nodes: "z[1-2]".into(),
                    max_time_s: None,
                    default: true,
                    hidden: false,
                    state: "UP",
                },
            ];
            for (record, expected) in records.iter().zip(&expected) {
                assert_eq!(&record.partition, expected, "{protocol:?}");
                assert_eq!(record.node_indices, [1, 2], "{protocol:?}");
            }
            assert_eq!(records.len(), 2);
        }
    }

    #[test]
    fn nodes_list_their_partitions_in_order_and_those_in_none_are_left_out() {
        let mut body = Pack::default();
        body.u32(3).time(0);
        for name in ["z1", "z2", "z3"] {
            v23_11_record(&mut body, Some(name));
        }
        let record = |name: &str, node_indices: Vec<u32>| PartitionRecord {
            partition: Partition {
                name: name.into(),
                nodes: String::new(),
                max_time_s: None,
                default: false,
                hidden: false,
                state: "UP",
            },
            node_indices,
        };
        let partitions = [record("long", vec![0, 2]), record("short", vec![0])];
        let found = nodes(&body.0, &partitions, Protocol::V23_11).unwrap();
        let listed: Vec<(&str, Vec<&str>)> = found
            .iter()
            .map(|node| {
                let partitions = node.partitions.iter().map(String::as_str);
                (node.name.as_str(), partitions.collect())
            })
            .collect();
        assert_eq!(
            listed,
            [("z1", vec!["long", "short"]), ("z3", vec!["long"])]
        );
    }
}
