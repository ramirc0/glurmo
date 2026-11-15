//! Node state, CPUs, load, memory and GRES from sinfo.

use std::collections::HashMap;

use anyhow::{Context, Result};

use super::fields::{
    columns, format_spec, gpu_count, gpu_model, gres, integer, is_placeholder, megabytes,
};
use rtviz_model::{Node, NodeFlag, NodeName, NodeState, PartitionName};

/// The `-O` fields, in the order [`parse`] reads them. Free text goes last.
pub const FIELDS: [&str; 14] = [
    "NodeList",
    "Partition",
    "StateComplete",
    "CPUsState",
    "CPUsLoad",
    "Memory",
    "AllocMem",
    "FreeMem",
    "Gres",
    "GresUsed",
    "Features",
    "Sockets",
    "Cores",
    "Reason",
];

/// The command. sinfo lists a node once per partition, and `-S N,#P` keeps partitions in config order.
pub fn argv() -> Vec<String> {
    ["sinfo", "-h", "--all", "-N", "-S", "N,#P", "-O"]
        .map(String::from)
        .into_iter()
        .chain([format_spec(&FIELDS)])
        .collect()
}

/// Parses the output of [`argv`] into one node per hostname, with its partitions merged.
///
/// # Errors
///
/// Fails on a line with too few columns or a value that does not parse.
pub fn parse(stdout: &str) -> Result<Vec<Node>> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut index: HashMap<NodeName, usize> = HashMap::new();
    for line in stdout.lines() {
        let node = node(line).with_context(|| format!("sinfo line {line:?}"))?;
        if let Some(&i) = index.get(&node.name) {
            nodes[i].partitions.extend(node.partitions);
        } else {
            index.insert(node.name.clone(), nodes.len());
            nodes.push(node);
        }
    }
    Ok(nodes)
}

fn node(line: &str) -> Result<Node> {
    let [
        name,
        partition,
        state,
        cpus,
        load,
        memory,
        alloc_mem,
        free_mem,
        gres_total,
        gres_used,
        features,
        sockets,
        cores_per_socket,
        reason,
    ] = columns(line)?;
    let (state_neutral, flags) = node_state(state);
    let [cpus_alloc, _idle, _other, cpus_total] = cpus
        .split('/')
        .map(integer::<u32>)
        .collect::<Result<Vec<_>>>()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("bad CPU states {cpus:?}"))?;
    let sockets: u32 = integer(sockets)?;
    let gres_total = gres(gres_total)?;
    Ok(Node {
        name: NodeName(name.to_owned()),
        partitions: vec![PartitionName(partition.trim_end_matches('*').to_owned())],
        state: state_neutral,
        flags,
        state_raw: state.to_owned(),
        reason: (!is_placeholder(reason)).then(|| reason.to_owned()),
        cpus: cpus_total,
        cpus_alloc,
        sockets,
        cores: sockets * integer::<u32>(cores_per_socket)?,
        load: if is_placeholder(load) {
            None
        } else {
            Some(load.parse().with_context(|| format!("bad load {load:?}"))?)
        },
        mem_mb: megabytes(memory)?.context("no memory size")?,
        mem_alloc_mb: megabytes(alloc_mem)?.unwrap_or(0),
        mem_free_mb: megabytes(free_mem)?,
        gpus: gpu_count(&gres_total),
        gpus_used: gpu_count(&gres(gres_used)?),
        gpu_model: gpu_model(&gres_total),
        features: if is_placeholder(features) {
            Vec::new()
        } else {
            features.split(',').map(str::to_owned).collect()
        },
    })
}

/// Maps a `StateComplete` value such as `mixed+drain+not_responding`.
/// Unknown base states map to [`NodeState::Unknown`], and unknown flags stay only in the raw string.
fn node_state(raw: &str) -> (NodeState, Vec<NodeFlag>) {
    let mut parts = raw.split('+');
    let base = parts.next().unwrap_or_default();
    let flags: Vec<&str> = parts.collect();
    let draining = flags.iter().any(|flag| matches!(*flag, "drain" | "fail"));
    let state = match base {
        "down" => NodeState::Down,
        "idle" if draining => NodeState::Drained,
        "mixed" | "allocated" if draining => NodeState::Draining,
        "idle" => NodeState::Idle,
        "mixed" => NodeState::Mixed,
        "allocated" => NodeState::Allocated,
        _ => NodeState::Unknown,
    };
    let mut neutral: Vec<NodeFlag> = flags
        .iter()
        .filter_map(|flag| match *flag {
            "not_responding" => Some(NodeFlag::NotResponding),
            "power_down" | "powered_down" | "powering_down" | "powering_up" => {
                Some(NodeFlag::PowerSaving)
            }
            "reserved" => Some(NodeFlag::Reserved),
            "maint" => Some(NodeFlag::Maintenance),
            _ => None,
        })
        .collect();
    neutral.dedup();
    (state, neutral)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "\
js029|gpujs029|idle+drain+invalid_reg|0/0/256/256|24.71|1031561|0|206460|gpu:8|gpu:0|(null)|2|64|Low RealMemory (reported:1031556 < 100.00% of configured:1031561)
z028|gpuh200|mixed|32/224/0/256|0.08|1000000|262144|689692|gpu:8|gpu:4|(null)|2|64|none
z1014|30mins*|idle|0/88/0/88|0.19|500000|0|254476|(null)|gpu:0|broadwell|2|22|none
z1014|4hours|idle|0/88/0/88|0.19|500000|0|254476|(null)|gpu:0|broadwell|2|22|none
z1014|5days|idle|0/88/0/88|0.19|500000|0|254476|(null)|gpu:0|broadwell|2|22|none
z1018|30mins*|down+not_responding|0/0/88/88|0.00|500000|0|242137|(null)|gpu:0|broadwell|2|22|Not responding
z1054|long|allocated|72/0/0/72|0.07|1000000|1000000|878437|(null)|gpu:0|skylake|2|18|none
z1079|4hours|down+not_responding|0/0/104/104|0.00|1000000|0|N/A|(null)|gpu:0|cascadelake|2|26|Not responding
z1080|4hours|idle+completing|0/104/0/104|377.20|1000000|0|552084|(null)|gpu:0|cascadelake|2|26|none
";

    fn node<'a>(nodes: &'a [Node], name: &str) -> &'a Node {
        nodes.iter().find(|node| node.name.0 == name).unwrap()
    }

    #[test]
    fn partitions_merge_in_config_order_without_the_default_mark() {
        let nodes = parse(LIVE).unwrap();
        assert_eq!(nodes.len(), 7);
        let partitions: Vec<&str> = node(&nodes, "z1014")
            .partitions
            .iter()
            .map(|partition| partition.0.as_str())
            .collect();
        assert_eq!(partitions, ["30mins", "4hours", "5days"]);
    }

    #[test]
    fn gpu_node_counts() {
        let nodes = parse(LIVE).unwrap();
        let z028 = node(&nodes, "z028");
        assert_eq!(z028.state, NodeState::Mixed);
        assert_eq!((z028.cpus, z028.cpus_alloc, z028.cores), (256, 32, 128));
        assert_eq!(z028.cpus_per_core(), 2);
        assert_eq!(
            (z028.gpus, z028.gpus_used, z028.gpu_model.as_deref()),
            (8, 4, None)
        );
        assert_eq!(z028.mem_alloc_mb, 262_144);
        assert_eq!(z028.reason, None);
        assert_eq!(z028.features, Vec::<String>::new());
    }

    #[test]
    fn states_and_flags() {
        let nodes = parse(LIVE).unwrap();
        let js029 = node(&nodes, "js029");
        assert_eq!(js029.state, NodeState::Drained);
        assert_eq!(js029.flags, Vec::new());
        assert_eq!(js029.state_raw, "idle+drain+invalid_reg");
        assert!(
            js029
                .reason
                .as_deref()
                .unwrap()
                .starts_with("Low RealMemory")
        );

        let z1018 = node(&nodes, "z1018");
        assert_eq!(
            (z1018.state, z1018.flags.as_slice()),
            (NodeState::Down, &[NodeFlag::NotResponding][..])
        );

        assert_eq!(node(&nodes, "z1054").state, NodeState::Allocated);
        assert_eq!(node(&nodes, "z1080").state, NodeState::Idle);
    }

    #[test]
    fn placeholders_in_numbers() {
        let nodes = parse(LIVE).unwrap();
        assert_eq!(node(&nodes, "z1079").mem_free_mb, None);
        assert_eq!(node(&nodes, "z1080").load, Some(377.2));
    }

    #[test]
    fn neutral_states_cover_draining_and_power_saving() {
        assert_eq!(node_state("mixed+drain"), (NodeState::Draining, Vec::new()));
        assert_eq!(
            node_state("idle+powered_down+reserved+maint"),
            (
                NodeState::Idle,
                vec![
                    NodeFlag::PowerSaving,
                    NodeFlag::Reserved,
                    NodeFlag::Maintenance
                ]
            )
        );
        assert_eq!(node_state("future"), (NodeState::Unknown, Vec::new()));
    }

    #[test]
    fn typed_gres_names_the_model() {
        let line = "g1|gpu|mixed|8/56/0/64|1.00|500000|1000|400000|gpu:a100:4(S:0-1)|gpu:a100:2(IDX:0,2)|(null)|2|16|none";
        let g1 = parse(line).unwrap().remove(0);
        assert_eq!(
            (g1.gpus, g1.gpus_used, g1.gpu_model.as_deref()),
            (4, 2, Some("a100"))
        );
    }

    #[test]
    fn short_lines_fail() {
        assert!(parse("z1|gpu|idle").is_err());
    }
}
