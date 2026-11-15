//! What the panels show, summed from a cluster and its occupancy. The terminal and web views share it.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::layout::{Grouping, QueueBy};
use crate::{
    Attempt, Cluster, Delta, Event, Login, Node, Occupancies, Occupancy, Owner, Partition,
    PartitionName, UnixTime,
};

/// How many cores or GPUs there are, and how many of them jobs hold.
///
/// # Examples
///
/// ```
/// use rtviz_model::summary::Tally;
///
/// let tally = Tally { total: 48, held: 36 };
/// assert_eq!(tally.free(), 12);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Cells counted.
    pub total: u64,
    /// Cells some job holds.
    pub held: u64,
}

impl Tally {
    /// Cells no job holds.
    ///
    /// # Examples
    ///
    /// ```
    /// use rtviz_model::summary::Tally;
    ///
    /// assert_eq!(Tally { total: 4, held: 1 }.free(), 3);
    /// ```
    pub fn free(self) -> u64 {
        self.total - self.held
    }

    fn add(&mut self, cells: &[Option<Owner>]) {
        self.total += cells.len() as u64;
        self.held += cells.iter().filter(|cell| cell.is_some()).count() as u64;
    }
}

/// Cores on nodes that can run jobs, and how many of them jobs hold.
///
/// # Examples
///
/// ```
/// use rtviz_model::Cluster;
/// use rtviz_model::summary::{Tally, cores};
///
/// let cluster = Cluster::default();
/// assert_eq!(cores(&cluster, &cluster.occupancy()), Tally::default());
/// ```
pub fn cores(cluster: &Cluster, occupancy: &Occupancies) -> Tally {
    usable_tally(cluster, occupancy, |cells| &cells.cores)
}

/// GPUs on nodes that can run jobs, and how many of them jobs hold.
///
/// # Examples
///
/// ```
/// use rtviz_model::Cluster;
/// use rtviz_model::summary::{Tally, gpus};
///
/// let cluster = Cluster::default();
/// assert_eq!(gpus(&cluster, &cluster.occupancy()), Tally::default());
/// ```
pub fn gpus(cluster: &Cluster, occupancy: &Occupancies) -> Tally {
    usable_tally(cluster, occupancy, |cells| &cells.gpus)
}

fn usable_tally(
    cluster: &Cluster,
    occupancy: &Occupancies,
    cells: impl Fn(&Occupancy) -> &Vec<Option<Owner>>,
) -> Tally {
    let mut tally = Tally::default();
    for node in cluster.nodes.iter().filter(|node| node.is_usable()) {
        if let Some(occupancy) = occupancy.get(&node.name) {
            tally.add(cells(occupancy));
        }
    }
    tally
}

/// What one user holds and waits for.
///
/// # Examples
///
/// ```
/// use rtviz_model::summary::Holding;
///
/// let holding = Holding { cores: 64, gpus: 2, jobs: 3, pending: 120 };
/// assert_eq!(holding.jobs, 3);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Holding {
    /// Cores held.
    pub cores: u64,
    /// GPUs held.
    pub gpus: u64,
    /// Attempts running.
    pub jobs: u64,
    /// Tasks waiting.
    pub pending: u64,
}

/// What each user holds and waits for, and what jobs the scheduler hides hold.
///
/// # Examples
///
/// ```
/// use rtviz_model::Cluster;
/// use rtviz_model::summary::holdings;
///
/// let cluster = Cluster::default();
/// let holdings = holdings(&cluster, &cluster.occupancy());
/// assert!(holdings.users.is_empty());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Holdings<'a> {
    /// One entry per user, most cores first, then most GPUs, then most tasks waiting.
    pub users: Vec<(&'a Login, Holding)>,
    /// Cores and GPUs that no visible job explains.
    pub hidden: Holding,
}

/// What each user holds and waits for, and what hidden jobs hold.
///
/// # Examples
///
/// ```
/// # let json = r#"{
/// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
/// #     "partitions": ["5days"], "state": "pending", "state_raw": "PENDING", "cpus": 8,
/// #     "mem_mb": null, "gpus": 0, "time_limit_s": null, "start": null, "end": null,
/// #     "nodes": [], "reason": "Priority", "name": "align"
/// # }"#;
/// use rtviz_model::{Attempt, Cluster};
/// use rtviz_model::summary::holdings;
///
/// let mut cluster = Cluster::default();
/// let attempt: Attempt = serde_json::from_str(json).unwrap();
/// cluster.queue.insert(attempt.key, attempt);
/// let holdings = holdings(&cluster, &cluster.occupancy());
/// assert_eq!(holdings.users[0].0.0, "otter");
/// assert_eq!(holdings.users[0].1.pending, 1);
/// ```
pub fn holdings<'a>(cluster: &'a Cluster, occupancy: &Occupancies) -> Holdings<'a> {
    let mut users: BTreeMap<&Login, Holding> = BTreeMap::new();
    let mut hidden = Holding::default();
    for cells in occupancy.values() {
        for (owners, gpus) in [(&cells.cores, false), (&cells.gpus, true)] {
            for owner in owners.iter().flatten() {
                let holding = match owner {
                    Owner::Hidden => &mut hidden,
                    Owner::Attempt(key) => match cluster.running.get(key) {
                        Some(running) => users.entry(&running.attempt.login).or_default(),
                        None => continue,
                    },
                };
                if gpus {
                    holding.gpus += 1;
                } else {
                    holding.cores += 1;
                }
            }
        }
    }
    for running in cluster.running.values() {
        users.entry(&running.attempt.login).or_default().jobs += 1;
    }
    for attempt in cluster.queue.values() {
        users.entry(&attempt.login).or_default().pending += u64::from(attempt.tasks());
    }
    let mut users: Vec<_> = users.into_iter().collect();
    users.sort_by_key(|(login, held)| {
        (
            std::cmp::Reverse((held.cores, held.gpus, held.pending)),
            *login,
        )
    });
    Holdings { users, hidden }
}

/// What waits in the queue under one partition or user.
///
/// # Examples
///
/// ```
/// use rtviz_model::summary::Waiting;
///
/// let waiting = Waiting { key: "5days", jobs: 2, tasks: 10, cpus: 80, gpus: 0, oldest: None, reason: "Priority" };
/// assert_eq!(waiting.cpus / waiting.tasks, 8);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Waiting<'a> {
    /// The partition or login.
    pub key: &'a str,
    /// Queue rows. A pending array range counts once.
    pub jobs: u64,
    /// Tasks, counting each task of an array range.
    pub tasks: u64,
    /// CPUs the tasks ask for.
    pub cpus: u64,
    /// GPUs the tasks ask for.
    pub gpus: u64,
    /// The earliest submit time.
    pub oldest: Option<UnixTime>,
    /// The reason most tasks wait for.
    pub reason: &'a str,
}

/// Pending attempts summed by their first partition or by login, most tasks first.
///
/// # Examples
///
/// ```
/// # let json = r#"{
/// #     "key": {"job": 4242, "submit": 1700000000}, "array": {"job": 4242, "spec": "0-3", "count": 4},
/// #     "login": "otter", "partitions": ["5days"], "state": "pending", "state_raw": "PENDING", "cpus": 8,
/// #     "mem_mb": null, "gpus": 0, "time_limit_s": null, "start": null, "end": null,
/// #     "nodes": [], "reason": "Priority", "name": "align"
/// # }"#;
/// use rtviz_model::Attempt;
/// use rtviz_model::layout::QueueBy;
/// use rtviz_model::summary::waiting;
///
/// let attempt: Attempt = serde_json::from_str(json).unwrap();
/// let rows = waiting([&attempt], QueueBy::Partition);
/// assert_eq!((rows[0].key, rows[0].tasks, rows[0].cpus), ("5days", 4, 32));
/// ```
pub fn waiting<'a>(
    attempts: impl IntoIterator<Item = &'a Attempt>,
    by: QueueBy,
) -> Vec<Waiting<'a>> {
    let mut groups: BTreeMap<&str, (Waiting, HashMap<&str, u64>)> = BTreeMap::new();
    for attempt in attempts {
        let key = match by {
            QueueBy::Partition => attempt.partitions.first().map_or("", |p| p.0.as_str()),
            QueueBy::Login => attempt.login.0.as_str(),
        };
        let tasks = u64::from(attempt.tasks());
        let (waiting, reasons) = groups.entry(key).or_default();
        waiting.key = key;
        waiting.jobs += 1;
        waiting.tasks += tasks;
        waiting.cpus += u64::from(attempt.cpus) * tasks;
        waiting.gpus += u64::from(attempt.gpus) * tasks;
        let submit = attempt.key.submit;
        waiting.oldest = Some(waiting.oldest.map_or(submit, |oldest| oldest.min(submit)));
        *reasons
            .entry(attempt.reason.as_deref().unwrap_or("-"))
            .or_default() += tasks;
    }
    let mut rows: Vec<Waiting> = groups
        .into_values()
        .map(|(mut waiting, reasons)| {
            waiting.reason = reasons
                .into_iter()
                .max_by_key(|(reason, tasks)| (*tasks, std::cmp::Reverse(*reason)))
                .map_or("", |(reason, _)| reason);
            waiting
        })
        .collect();
    rows.sort_by_key(|waiting| (std::cmp::Reverse(waiting.tasks), waiting.key));
    rows
}

/// A partition's jobs and free cells.
///
/// # Examples
///
/// ```
/// use rtviz_model::Cluster;
/// use rtviz_model::summary::partition_loads;
///
/// let cluster = Cluster::default();
/// assert!(partition_loads(&cluster, &cluster.occupancy()).is_empty());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionLoad<'a> {
    /// The partition.
    pub partition: &'a Partition,
    /// Attempts running with it as their first partition.
    pub running: u64,
    /// Tasks waiting with it as their first partition.
    pub pending: u64,
    /// Free cores on its usable nodes.
    pub free_cores: u64,
    /// Free GPUs on its usable nodes.
    pub free_gpus: u64,
}

/// Each partition's load, in the scheduler's order.
///
/// # Examples
///
/// ```
/// use rtviz_model::{Cluster, Partition, PartitionName};
/// use rtviz_model::summary::partition_loads;
///
/// let partition = Partition {
///     name: PartitionName("5days".into()),
///     nodes: Vec::new(),
///     max_time_s: None,
///     default: true,
///     hidden: false,
///     state_raw: "UP".into(),
/// };
/// let cluster = Cluster { partitions: vec![partition], ..Cluster::default() };
/// let loads = partition_loads(&cluster, &cluster.occupancy());
/// assert_eq!((loads[0].running, loads[0].free_cores), (0, 0));
/// ```
pub fn partition_loads<'a>(
    cluster: &'a Cluster,
    occupancy: &Occupancies,
) -> Vec<PartitionLoad<'a>> {
    let mut running: HashMap<&PartitionName, u64> = HashMap::new();
    for attempt in cluster.running.values().map(|running| &running.attempt) {
        if let Some(partition) = attempt.partitions.first() {
            *running.entry(partition).or_default() += 1;
        }
    }
    let mut pending: HashMap<&PartitionName, u64> = HashMap::new();
    for attempt in cluster.queue.values() {
        if let Some(partition) = attempt.partitions.first() {
            *pending.entry(partition).or_default() += u64::from(attempt.tasks());
        }
    }
    cluster
        .partitions
        .iter()
        .map(|partition| {
            let members: HashSet<_> = partition.nodes.iter().collect();
            let (mut cores, mut gpus) = (Tally::default(), Tally::default());
            for node in cluster
                .nodes
                .iter()
                .filter(|node| members.contains(&node.name) && node.is_usable())
            {
                if let Some(cells) = occupancy.get(&node.name) {
                    cores.add(&cells.cores);
                    gpus.add(&cells.gpus);
                }
            }
            PartitionLoad {
                partition,
                running: running.get(&partition.name).copied().unwrap_or(0),
                pending: pending.get(&partition.name).copied().unwrap_or(0),
                free_cores: cores.free(),
                free_gpus: gpus.free(),
            }
        })
        .collect()
}

/// Nodes drawn together under one title.
///
/// # Examples
///
/// ```
/// use rtviz_model::summary::NodeGroup;
///
/// let group = NodeGroup { name: "gpu · V100".into(), nodes: Vec::new() };
/// assert!(group.nodes.is_empty());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct NodeGroup<'a> {
    /// The title.
    pub name: String,
    /// Its nodes.
    pub nodes: Vec<&'a Node>,
}

impl NodeGroup<'_> {
    /// The group's cores, and how many of them jobs hold, counting nodes that are out.
    ///
    /// # Examples
    ///
    /// ```
    /// use rtviz_model::Cluster;
    /// use rtviz_model::summary::{NodeGroup, Tally};
    ///
    /// let group = NodeGroup { name: "cpu".into(), nodes: Vec::new() };
    /// assert_eq!(group.cores(&Cluster::default().occupancy()), Tally::default());
    /// ```
    pub fn cores(&self, occupancy: &Occupancies) -> Tally {
        self.tally(occupancy, |cells| &cells.cores)
    }

    /// The group's GPUs, and how many of them jobs hold, counting nodes that are out.
    ///
    /// # Examples
    ///
    /// ```
    /// use rtviz_model::Cluster;
    /// use rtviz_model::summary::{NodeGroup, Tally};
    ///
    /// let group = NodeGroup { name: "cpu".into(), nodes: Vec::new() };
    /// assert_eq!(group.gpus(&Cluster::default().occupancy()), Tally::default());
    /// ```
    pub fn gpus(&self, occupancy: &Occupancies) -> Tally {
        self.tally(occupancy, |cells| &cells.gpus)
    }

    fn tally(
        &self,
        occupancy: &Occupancies,
        cells: impl Fn(&Occupancy) -> &Vec<Option<Owner>>,
    ) -> Tally {
        let mut tally = Tally::default();
        for node in &self.nodes {
            if let Some(occupancy) = occupancy.get(&node.name) {
                tally.add(cells(occupancy));
            }
        }
        tally
    }
}

/// The nodes in groups. By hardware, CPU nodes group by their features, most cores first, and GPU
/// nodes follow by GPU model and core count. By partition, a node shows once in each of its partitions.
///
/// # Examples
///
/// ```
/// use rtviz_model::Cluster;
/// use rtviz_model::layout::Grouping;
/// use rtviz_model::summary::node_groups;
///
/// assert!(node_groups(&Cluster::default(), Grouping::Hardware).is_empty());
/// ```
pub fn node_groups(cluster: &Cluster, grouping: Grouping) -> Vec<NodeGroup<'_>> {
    let nodes = &cluster.nodes;
    match grouping {
        Grouping::Hardware => {
            let mut cpu: BTreeMap<String, Vec<&Node>> = BTreeMap::new();
            let mut gpu: BTreeMap<(String, u32), Vec<&Node>> = BTreeMap::new();
            for node in nodes {
                if node.gpus > 0 {
                    let name = node
                        .gpu_model
                        .as_ref()
                        .map_or_else(|| "gpu".to_owned(), |model| format!("gpu · {model}"));
                    gpu.entry((name, node.cores)).or_default().push(node);
                } else {
                    let name = if node.features.is_empty() {
                        "cpu".to_owned()
                    } else {
                        node.features.join(" + ")
                    };
                    cpu.entry(name).or_default().push(node);
                }
            }
            let mut groups: Vec<NodeGroup> = cpu
                .into_iter()
                .map(|(name, nodes)| NodeGroup { name, nodes })
                .collect();
            groups.sort_by_key(|group| {
                std::cmp::Reverse(group.nodes.iter().map(|node| node.cores).sum::<u32>())
            });
            groups.extend(
                gpu.into_iter()
                    .map(|((name, _), nodes)| NodeGroup { name, nodes }),
            );
            groups
        }
        Grouping::Partition => cluster
            .partitions
            .iter()
            .map(|partition| NodeGroup {
                name: partition.name.0.clone(),
                nodes: nodes
                    .iter()
                    .filter(|node| partition.nodes.contains(&node.name))
                    .collect(),
            })
            .filter(|group| !group.nodes.is_empty())
            .collect(),
    }
}

/// An attempt taking or freeing its cores, for a ticker. It carries no outcome.
///
/// # Examples
///
/// ```
/// use rtviz_model::{Login, UnixTime};
/// use rtviz_model::summary::Tick;
///
/// let tick = Tick { at: UnixTime(0), started: true, login: Login("otter".into()), partition: None, cpus: 8, gpus: 0 };
/// assert!(tick.started);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tick {
    /// When it started or ended.
    pub at: UnixTime,
    /// Whether it started, as opposed to ended.
    pub started: bool,
    /// The owner.
    pub login: Login,
    /// The partition it runs in.
    pub partition: Option<PartitionName>,
    /// CPUs it holds.
    pub cpus: u32,
    /// GPUs it holds.
    pub gpus: u32,
}

impl Tick {
    fn of(attempt: &Attempt, at: UnixTime, started: bool) -> Self {
        Self {
            at,
            started,
            login: attempt.login.clone(),
            partition: attempt.partitions.first().cloned(),
            cpus: attempt.cpus,
            gpus: attempt.gpus,
        }
    }
}

/// The starts and ends in `delta`, read against `cluster` before the delta applies. An attempt that
/// starts and ends within one delta gives both.
///
/// # Examples
///
/// ```
/// # let json = r#"{
/// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
/// #     "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
/// #     "mem_mb": null, "gpus": 0, "time_limit_s": null, "start": 1700000100, "end": null,
/// #     "nodes": ["z1001"], "reason": null, "name": "align"
/// # }"#;
/// use rtviz_model::{Attempt, Cluster, Delta, Event, UnixTime};
/// use rtviz_model::summary::ticks;
///
/// let attempt: Attempt = serde_json::from_str(json).unwrap();
/// let key = attempt.key;
/// let events = vec![Event::Started { attempt }, Event::Ended { key, at: UnixTime(1_700_000_200), state: None }];
/// let ticks = ticks(&Cluster::default(), &Delta { seq: 1, at: UnixTime(1_700_000_200), events });
/// assert_eq!(ticks.iter().map(|tick| tick.started).collect::<Vec<_>>(), [true, false]);
/// ```
pub fn ticks(cluster: &Cluster, delta: &Delta) -> Vec<Tick> {
    let mut started: HashMap<_, &Attempt> = HashMap::new();
    let mut ticks = Vec::new();
    for event in &delta.events {
        match event {
            Event::Started { attempt } => {
                started.insert(attempt.key, attempt);
                ticks.push(Tick::of(attempt, attempt.start.unwrap_or(delta.at), true));
            }
            Event::Ended { key, at, .. } => {
                let attempt = started
                    .get(key)
                    .copied()
                    .or_else(|| cluster.running.get(key).map(|running| &running.attempt));
                if let Some(attempt) = attempt {
                    ticks.push(Tick::of(attempt, *at, false));
                }
            }
            _ => {}
        }
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NodeName, NodeState};

    fn node(name: &str, cores: u32, gpus: u32) -> Node {
        Node {
            name: NodeName(name.into()),
            partitions: Vec::new(),
            state: NodeState::Idle,
            flags: Vec::new(),
            state_raw: "IDLE".into(),
            reason: None,
            cpus: cores * 2,
            cpus_alloc: 0,
            sockets: 2,
            cores,
            load: None,
            mem_mb: 0,
            mem_alloc_mb: 0,
            mem_free_mb: None,
            gpus,
            gpus_used: 0,
            gpu_model: None,
            features: Vec::new(),
        }
    }

    #[test]
    fn gpu_nodes_with_no_model_group_by_core_count() {
        let mut typed = node("t1", 16, 4);
        typed.gpu_model = Some("V100".into());
        let cluster = Cluster {
            nodes: vec![
                node("c1", 26, 0),
                node("big", 128, 8),
                node("g1", 16, 4),
                typed,
                node("g2", 16, 4),
            ],
            ..Cluster::default()
        };
        let groups: Vec<(String, usize)> = node_groups(&cluster, Grouping::Hardware)
            .iter()
            .map(|group| (group.name.clone(), group.nodes.len()))
            .collect();
        let expected = [("cpu", 1), ("gpu", 2), ("gpu", 1), ("gpu · V100", 1)];
        assert_eq!(
            groups,
            expected.map(|(name, count)| (name.to_owned(), count))
        );
    }
}
