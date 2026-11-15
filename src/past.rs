//! The cluster at a past moment, rebuilt from history, and the changes from one moment to another.
//!
//! History keeps who held which cores and memory, the queue, and node health. It keeps no load, free
//! memory, pending reasons, job names or scheduler counters, so the past leaves them out. Hardware, partitions
//! and users come from the present.

use std::collections::{BTreeMap, HashMap};

use crate::store::{NodeHealth, Stored};
use glurmo_model::{
    Attempt, Cluster, Delta, Event, JobState, Login, Node, NodeFlag, NodeName, NodeState,
    Partition, Running, UnixTime, User,
};

/// What the past borrows from the present, because history does not keep it.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Every node, for its hardware.
    pub nodes: Vec<Node>,
    /// Every partition.
    pub partitions: Vec<Partition>,
    /// Everyone seen so far.
    pub users: BTreeMap<Login, User>,
}

impl Frame {
    /// The frame of `cluster`.
    pub fn of(cluster: &Cluster) -> Self {
        Self {
            nodes: cluster.nodes.clone(),
            partitions: cluster.partitions.clone(),
            users: cluster.users.clone(),
        }
    }
}

/// The cluster at `at`, from the attempts `alive` then and every health record up to then.
pub fn cluster_at(
    at: UnixTime,
    alive: Vec<Stored>,
    health: &[NodeHealth],
    frame: &Frame,
) -> Cluster {
    let mut running = BTreeMap::new();
    let mut queue = BTreeMap::new();
    for stored in alive {
        let key = stored.attempt.key;
        if stored.start.is_some_and(|start| start <= at) {
            running.insert(key, holding(&stored));
        } else {
            queue.insert(key, stored.attempt);
        }
    }
    let mut cluster = Cluster {
        at: Some(at),
        nodes: nodes_at(at, health, frame),
        partitions: frame.partitions.clone(),
        running,
        queue,
        users: frame.users.clone(),
        ..Cluster::default()
    };
    cluster.tally_allocation();
    cluster
}

/// The changes after `from` and up to `to`, one delta per second in which something changed, from the
/// attempts `touched` then and every health record up to `to`. Applied to the cluster at `from`, each
/// followed by [`Cluster::tally_allocation`], they give the cluster at `to`.
pub fn deltas(
    from: UnixTime,
    to: UnixTime,
    touched: Vec<Stored>,
    health: &[NodeHealth],
    frame: &Frame,
) -> Vec<Delta> {
    let within = |at: UnixTime| from < at && at <= to;
    // Each event with its time and its place in an attempt's life: queued, then started, then ended.
    let mut events: Vec<(UnixTime, u8, Event)> = Vec::new();
    for stored in touched {
        let key = stored.attempt.key;
        if within(key.submit) {
            events.push((key.submit, 0, Event::Queued(vec![stored.attempt.clone()])));
        }
        if let Some(start) = stored.start.filter(|start| within(*start)) {
            events.push((start, 1, Event::Dequeued(vec![key])));
            events.push((
                start,
                1,
                Event::Started {
                    attempt: holding(&stored).attempt,
                },
            ));
            if let Some(placement) = stored.placement.clone() {
                events.push((start, 1, Event::Placed { key, placement }));
            }
        }
        if let Some(end) = stored.end.filter(|end| within(*end)) {
            let ended = match stored.start {
                Some(_) => Event::Ended {
                    key,
                    at: end,
                    state: stored.state,
                },
                None => Event::Dequeued(vec![key]),
            };
            events.push((end, 2, ended));
        }
    }
    let mut changes: Vec<UnixTime> = health
        .iter()
        .map(|record| record.at)
        .filter(|at| within(*at))
        .collect();
    changes.dedup();
    for at in changes {
        events.push((at, 3, Event::Nodes(nodes_at(at, health, frame))));
    }
    events.sort_by_key(|(at, order, _)| (*at, *order));

    let mut deltas: Vec<Delta> = Vec::new();
    for (at, _, event) in events {
        match deltas.last_mut() {
            Some(delta) if delta.at == at => delta.events.push(event),
            _ => deltas.push(Delta {
                seq: deltas.len() as u64 + 1,
                at,
                events: vec![event],
            }),
        }
    }
    deltas
}

/// The attempt as it held its cores.
fn holding(stored: &Stored) -> Running {
    Running {
        attempt: Attempt {
            state: JobState::Running,
            state_raw: "RUNNING".to_owned(),
            start: stored.start,
            ..stored.attempt.clone()
        },
        placement: stored.placement.clone(),
    }
}

/// The frame's nodes, each with its last health up to `at`. A node with no record by then counts as up.
/// Allocation, load and free memory are left empty.
fn nodes_at(at: UnixTime, health: &[NodeHealth], frame: &Frame) -> Vec<Node> {
    let mut last: HashMap<&NodeName, &NodeHealth> = HashMap::new();
    for record in health.iter().take_while(|record| record.at <= at) {
        last.insert(&record.node, record);
    }
    frame
        .nodes
        .iter()
        .map(|node| {
            let record = last.get(&node.name);
            let state = record.and_then(|record| record.state);
            Node {
                state: state.unwrap_or(NodeState::Idle),
                flags: record
                    .filter(|record| record.not_responding)
                    .map(|_| NodeFlag::NotResponding)
                    .into_iter()
                    .collect(),
                state_raw: state
                    .map_or_else(String::new, |state| format!("{state:?}").to_uppercase()),
                reason: record.and_then(|record| record.reason.clone()),
                cpus_alloc: 0,
                load: None,
                mem_alloc_mb: 0,
                mem_free_mb: None,
                gpus_used: 0,
                ..node.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crate::fixture::busy_store;
    use glurmo_model::{AttemptKey, Occupancies};

    /// Cores held on each node, by count.
    fn held(occupancy: &Occupancies) -> BTreeMap<NodeName, usize> {
        occupancy
            .iter()
            .map(|(node, cells)| (node.clone(), cells.cores.iter().flatten().count()))
            .collect()
    }

    #[tokio::test]
    async fn a_node_that_stopped_answering_shows_no_reply_in_the_past() {
        let (_home, hub, _) = busy_store().await;
        let frame = hub.read(Frame::of);
        let name = frame.nodes[0].name.clone();
        let health = [NodeHealth {
            node: name.clone(),
            at: UnixTime(10),
            state: None,
            not_responding: true,
            reason: None,
        }];
        let nodes = nodes_at(UnixTime(10), &health, &frame);
        let node = nodes.iter().find(|node| node.name == name).unwrap();
        assert_eq!(node.trouble(), Some("no reply"));
    }

    #[tokio::test]
    async fn the_past_plus_its_changes_is_the_later_past() {
        let (_home, hub, history) = busy_store().await;
        let frame = hub.read(Frame::of);
        let ticks: Vec<UnixTime> = hub.deltas().iter().map(|delta| delta.at).collect();
        let (first, last) = (ticks[0], *ticks.last().unwrap());
        let middle = UnixTime(first.0.midpoint(last.0));
        let before = UnixTime(first.0 - 60);
        let after = UnixTime(last.0 + 60);
        let past = |at| {
            let health = history.node_health(at).unwrap();
            cluster_at(at, history.alive(at).unwrap(), &health, &frame)
        };
        for (from, to) in [
            (before, middle),
            (middle, after),
            (first, last),
            (before, after),
        ] {
            let health = history.node_health(to).unwrap();
            let changes = deltas(
                from,
                to,
                history.touched(from, to).unwrap(),
                &health,
                &frame,
            );
            assert!(
                changes.len() > 10,
                "{from:?} to {to:?} has {} deltas",
                changes.len()
            );
            assert!(changes.windows(2).all(|pair| pair[0].at < pair[1].at));
            let mut cluster = past(from);
            for delta in &changes {
                cluster.apply(delta);
                cluster.tally_allocation();
            }
            cluster.at = Some(to);
            let expected = past(to);
            assert_ne!(past(from).running, expected.running);
            assert_eq!(cluster.running, expected.running, "{from:?} to {to:?}");
            assert_eq!(cluster.queue, expected.queue, "{from:?} to {to:?}");
            assert_eq!(cluster, expected, "{from:?} to {to:?}");
        }
    }

    /// A tick's listing runs a moment after the tick starts, so it may or may not see an attempt that
    /// starts or ends within that second. Those are left out of both sides, and so is the memory on their
    /// nodes and on completing nodes.
    #[tokio::test]
    async fn the_past_holds_the_cores_and_memory_the_stream_showed() {
        let (_home, hub, history) = busy_store().await;
        let frame = hub.read(Frame::of);
        let times: HashMap<AttemptKey, (Option<UnixTime>, Option<UnixTime>)> = history
            .touched(UnixTime(0), UnixTime(i64::MAX))
            .unwrap()
            .into_iter()
            .map(|stored| (stored.attempt.key, (stored.start, stored.end)))
            .collect();
        let mut live = Cluster::default();
        let (mut ticks, mut unsure) = (0, 0);
        for delta in hub.deltas() {
            live.apply(&delta);
            let at = delta.at;
            let health = history.node_health(at).unwrap();
            let mut past = cluster_at(at, history.alive(at).unwrap(), &health, &frame);
            let mut seen = live.clone();
            let mut unsure_nodes = HashSet::new();
            for cluster in [&mut past, &mut seen] {
                cluster.running.retain(|key, running| {
                    let (start, end) = times[key];
                    let sure = start != Some(at) && end != Some(at);
                    if !sure {
                        unsure += 1;
                        unsure_nodes.extend(running.attempt.nodes.iter().cloned());
                    }
                    sure
                });
            }
            // sinfo's AllocMem leaves out jobs that start on a node while another there completes.
            unsure_nodes.extend(
                seen.nodes
                    .iter()
                    .filter(|node| node.state_raw.contains("completing"))
                    .map(|node| node.name.clone()),
            );
            let mem_held = |cluster: &Cluster| {
                cluster
                    .nodes
                    .iter()
                    .filter(|node| !unsure_nodes.contains(&node.name))
                    .map(|node| (node.name.clone(), node.mem_alloc_mb))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                mem_held(&past),
                mem_held(&seen),
                "memory held per node at {at:?}"
            );
            assert_eq!(
                past.running.keys().collect::<Vec<_>>(),
                seen.running.keys().collect::<Vec<_>>(),
                "attempts running at {at:?}"
            );
            assert_eq!(
                held(&past.occupancy()),
                held(&seen.occupancy()),
                "cores held per node at {at:?}"
            );
            ticks += 1;
        }
        assert!(ticks > 50, "{ticks} ticks");
        assert!(
            unsure < ticks * 10,
            "{unsure} attempts in their tick's second"
        );
    }
}
