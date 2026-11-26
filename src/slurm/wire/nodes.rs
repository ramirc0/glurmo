//! Nodes as sinfo lists them and partitions as scontrol shows them.

use std::collections::HashMap;

use anyhow::Result;
use slurm_client::Protocol;

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

/// The partition as scontrol shows it.
pub fn partition(partition: &slurm_client::Partition) -> Partition {
    use slurm_client::PartitionFlags as Flags;
    Partition {
        name: partition.name.clone(),
        nodes: partition.nodes.as_str().to_owned(),
        max_time_s: partition
            .max_time
            .and_then(slurm_client::Limit::finite)
            .map(|limit| limit.as_secs()),
        default: partition.flags.contains(Flags::DEFAULT),
        hidden: partition.flags.contains(Flags::HIDDEN),
        state: partition_state(partition.state),
    }
}

/// Names a partition state as scontrol does.
fn partition_state(state: slurm_client::PartitionState) -> &'static str {
    use slurm_client::PartitionState as State;
    match state {
        State::Up => "UP",
        State::Down => "DOWN",
        State::Drain => "DRAIN",
        State::Inactive => "INACTIVE",
        _ => "UNKNOWN",
    }
}

/// One node per record, with each node's partitions in configuration order, as `sinfo -N -S N,#P`
/// lists them. Like sinfo, it leaves out nodes in no partition.
///
/// # Errors
///
/// Fails when a partition's hostlist does not expand.
pub fn nodes(
    records: Vec<slurm_client::Node>,
    partitions: &[slurm_client::Partition],
    protocol: Protocol,
) -> Result<Vec<Node>> {
    let members = partitions
        .iter()
        .map(|partition| Ok((partition.name.as_str(), partition.nodes.expand()?)))
        .collect::<Result<Vec<_>>>()?;
    let nodes = records
        .into_iter()
        .filter_map(|node| raw(node).node(protocol))
        .collect();
    Ok(in_partitions(nodes, &members))
}

/// Gives each node its partitions, in the order of `members`, and drops nodes in none. The
/// controller's node indices cannot join the two, because 25.11 sends no record for a hidden node.
fn in_partitions(mut nodes: Vec<Node>, members: &[(&str, Vec<String>)]) -> Vec<Node> {
    let index: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.name.clone(), i))
        .collect();
    for (partition, hosts) in members {
        for host in hosts {
            if let Some(&i) = index.get(host) {
                nodes[i].partitions.push((*partition).to_owned());
            }
        }
    }
    nodes.retain(|node| !node.partitions.is_empty());
    nodes
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
    cpu_load: Option<f64>,
    free_mem: Option<u64>,
    alloc_cpus: u16,
    alloc_memory: u64,
    features: Vec<String>,
    gres_total: String,
    gres_used: String,
    reason: Option<String>,
}

fn raw(node: slurm_client::Node) -> Raw {
    let gres =
        |gres: Option<slurm_client::Gres>| gres.map_or_default(|gres| gres.as_str().to_owned());
    Raw {
        name: Some(node.name).filter(|name| !name.is_empty()),
        state: state_bits(node.state),
        cpus: node.cpus,
        sockets: node.sockets,
        cores_per_socket: node.cores_per_socket,
        real_memory: node.real_memory,
        cpu_load: node.cpu_load,
        free_mem: node.free_memory,
        alloc_cpus: node.alloc_cpus,
        alloc_memory: node.alloc_memory,
        features: node.features,
        gres_total: gres(node.gres),
        gres_used: gres(node.gres_used),
        reason: node.reason,
    }
}

/// A node state as Slurm numbers it: the base state in the low four bits, then the flags. The
/// library has already marked a partly allocated node mixed.
fn state_bits(state: slurm_client::NodeState) -> u32 {
    use slurm_client::NodeBaseState as Base;
    let base = match state.base {
        Base::Unknown => 0,
        Base::Down => 1,
        Base::Idle => 2,
        Base::Allocated => 3,
        Base::Error => 4,
        Base::Mixed => 5,
        Base::Future => 6,
        Base::Other(other) => other,
        _ => STATE_BASE,
    };
    base | state.flags.bits()
}

impl Raw {
    /// The node, or `None` for a blank record, which sinfo skips too.
    fn node(self, protocol: Protocol) -> Option<Node> {
        Some(Node {
            name: self.name?,
            partitions: Vec::new(),
            state: state_complete(self.state, protocol),
            reason: self.reason.filter(|reason| !reason.is_empty()),
            cpus: u32::from(self.cpus),
            cpus_alloc: u32::from(self.alloc_cpus),
            sockets: u32::from(self.sockets),
            cores: u32::from(self.sockets) * u32::from(self.cores_per_socket),
            load: self.cpu_load,
            mem_mb: self.real_memory,
            mem_alloc_mb: self.alloc_memory,
            mem_free_mb: self.free_mem,
            gres: self.gres_total,
            gres_used: self.gres_used,
            features: self.features,
        })
    }
}

const STATE_BASE: u32 = 0xf;

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
    let flags_in_order: &[(u32, &str)] = if protocol >= Protocol::V25_11 {
        &FLAGS_25_11
    } else {
        &FLAGS_23_11
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

    /// A draining, reserved node with a distinct value in each field glurmo reads.
    fn raw() -> Raw {
        Raw {
            name: Some("z1".into()),
            state: 3 | 1 << 9 | 1 << 5,
            cpus: 16,
            sockets: 2,
            cores_per_socket: 4,
            real_memory: 64_000,
            cpu_load: Some(2.5),
            free_mem: Some(30_000),
            alloc_cpus: 16,
            alloc_memory: 48_000,
            features: vec!["skylake".into(), "avx512".into()],
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
            cpu_load: None,
            free_mem: None,
            reason: Some(String::new()),
            features: Vec::new(),
            ..raw()
        };
        let node = unknown.node(Protocol::V23_11).unwrap();
        assert_eq!((node.load, node.mem_free_mb), (None, None));
        assert_eq!(node.reason, None);
        assert_eq!(node.features, Vec::<String>::new());
    }

    #[test]
    fn nodes_list_their_partitions_in_order_and_those_in_none_are_left_out() {
        let nodes = ["z1", "z2", "z3"]
            .map(|name| {
                let named = Raw {
                    name: Some(name.into()),
                    ..raw()
                };
                named.node(Protocol::V23_11).unwrap()
            })
            .to_vec();
        let members = [
            (
                "long",
                vec!["z1".to_owned(), "z3".to_owned(), "gone".to_owned()],
            ),
            ("short", vec!["z1".to_owned()]),
        ];
        let found = in_partitions(nodes, &members);
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

    #[test]
    fn a_blank_record_is_no_node() {
        let blank = Raw {
            name: None,
            ..raw()
        };
        assert!(blank.node(Protocol::V23_11).is_none());
    }
}
