//! Scheduler-neutral types that the Slurm module returns and the views consume.

use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A job ID unique to each array task, as sacct's `JobIDRaw` prints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct JobId(pub u32);

/// Seconds since the Unix epoch.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct UnixTime(pub i64);

impl UnixTime {
    /// Reads the system clock. A clock set before 1970 reads as 0.
    pub fn now() -> Self {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Self(i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
    }
}

/// One run of a job. A requeue keeps the job ID but resets the submit time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct AttemptKey {
    /// The job ID.
    pub job: JobId,
    /// When this attempt was submitted.
    pub submit: UnixTime,
}

/// A user's login name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Login(pub String);

/// A compute node's hostname.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct NodeName(pub String);

/// A partition, or a queue in schedulers that call it that.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PartitionName(pub String);

/// Where a job is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for resources, a dependency or a hold.
    Pending,
    /// Holding its cores.
    Running,
    /// Stopped by an operator. Its cores are free, but its memory is not.
    Suspended,
    /// Its cores are free, and its final state is not known yet.
    Completing,
    /// Finished.
    Ended(EndState),
}

/// How a job finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndState {
    /// Exited with code 0.
    Completed,
    /// Exited with a nonzero code or failed to launch.
    Failed,
    /// Cancelled by its owner or an operator.
    Cancelled,
    /// Reached its time limit or deadline.
    Timeout,
    /// Killed for exceeding its memory.
    OutOfMemory,
    /// Ended because a node failed.
    NodeFail,
    /// Preempted by a higher-priority job.
    Preempted,
    /// Ended to run again under the same job ID.
    Requeued,
    /// Left the scheduler unseen, and accounting holds no end for it.
    Lost,
}

/// The tasks of a job array that one queue row stands for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ArrayTasks {
    /// The array's base job ID.
    pub job: JobId,
    /// Task IDs as the scheduler prints them, such as `174` or `0-3,8%2`.
    pub spec: String,
    /// How many tasks the spec covers.
    pub count: u32,
}

/// A job as the queue lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Attempt {
    /// The job ID and submit time.
    pub key: AttemptKey,
    /// The array tasks this row stands for, if the job is part of an array.
    pub array: Option<ArrayTasks>,
    /// The owner.
    pub login: Login,
    /// The partitions it may run in, in the order the user gave.
    pub partitions: Vec<PartitionName>,
    /// The neutral state.
    pub state: JobState,
    /// The state as the scheduler prints it.
    pub state_raw: String,
    /// CPUs allocated, or requested while pending.
    pub cpus: u32,
    /// Memory allocated, or requested while pending.
    pub mem_mb: Option<u64>,
    /// GPUs allocated, or requested while pending.
    pub gpus: u32,
    /// The time limit, or `None` for no limit.
    pub time_limit_s: Option<u64>,
    /// When it started. While pending, the scheduler's estimate.
    pub start: Option<UnixTime>,
    /// When it ended or is expected to end.
    pub end: Option<UnixTime>,
    /// The nodes it holds.
    pub nodes: Vec<NodeName>,
    /// Why it waits or why it ended.
    pub reason: Option<String>,
    /// The job name.
    pub name: String,
}

/// A node's state, neutral across schedulers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// No cores allocated.
    Idle,
    /// Some cores allocated.
    Mixed,
    /// Every core allocated.
    Allocated,
    /// Set to drain while jobs still run on it.
    Draining,
    /// Drained and empty.
    Drained,
    /// Down.
    Down,
    /// A state the scheduler reports that none of the above covers.
    Unknown,
}

/// A condition that can hold in any node state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeFlag {
    /// The controller has lost contact with the node.
    NotResponding,
    /// Powered down, or powering down or up, to save power.
    PowerSaving,
    /// In a reservation.
    Reserved,
    /// In a maintenance reservation.
    Maintenance,
}

/// A compute node.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Node {
    /// The hostname.
    pub name: NodeName,
    /// The partitions that contain it, in config order.
    pub partitions: Vec<PartitionName>,
    /// The neutral state.
    pub state: NodeState,
    /// Conditions on top of the state.
    pub flags: Vec<NodeFlag>,
    /// The state as the scheduler prints it.
    pub state_raw: String,
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
    /// The load average, refreshed every few minutes.
    pub load: Option<f64>,
    /// Memory.
    pub mem_mb: u64,
    /// Memory allocated to jobs.
    pub mem_alloc_mb: u64,
    /// Memory free in the OS, refreshed every few minutes.
    pub mem_free_mb: Option<u64>,
    /// GPUs.
    pub gpus: u32,
    /// GPUs allocated to jobs.
    pub gpus_used: u32,
    /// The GPU model, when the scheduler names a single one.
    pub gpu_model: Option<String>,
    /// Feature tags, often the CPU generation.
    pub features: Vec<String>,
}

impl Node {
    /// How many CPUs make up one core: the threads per core, or 1 when the node schedules whole cores as CPUs.
    pub fn cpus_per_core(&self) -> u32 {
        self.cpus.checked_div(self.cores).unwrap_or(1).max(1)
    }
}

/// A partition and its nodes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Partition {
    /// The name.
    pub name: PartitionName,
    /// Its nodes.
    pub nodes: Vec<NodeName>,
    /// The longest time limit a job may ask for, or `None` for no limit.
    pub max_time_s: Option<u64>,
    /// Whether jobs that name no partition land here.
    pub default: bool,
    /// Whether the scheduler hides it from plain listings.
    pub hidden: bool,
    /// The state as the scheduler prints it, such as `UP`.
    pub state_raw: String,
}

/// The cores, GPUs and memory a job holds on one node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NodeCells {
    /// The node.
    pub node: NodeName,
    /// Core indices on the node.
    pub cores: Vec<u32>,
    /// Cores the job holds whose indices are unknown.
    pub unplaced: u32,
    /// GPU indices on the node.
    pub gpus: Vec<u32>,
    /// Memory on the node.
    pub mem_mb: Option<u64>,
}

/// Where a job's cores and GPUs are, node by node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Placement(pub Vec<NodeCells>);

impl Placement {
    /// Whether every core's index is known.
    pub fn is_exact(&self) -> bool {
        self.0.iter().all(|cells| cells.unplaced == 0)
    }
}

/// Counters since the controller's last daily reset. A counter this Slurm version does not print is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    /// Controller threads busy with RPCs.
    pub server_threads: Option<u64>,
    /// Outgoing messages waiting in the agent queue.
    pub agent_queue: Option<u64>,
    /// Accounting messages waiting for slurmdbd.
    pub dbd_queue: Option<u64>,
    /// Jobs submitted.
    pub jobs_submitted: Option<u64>,
    /// Jobs started.
    pub jobs_started: Option<u64>,
    /// Jobs completed.
    pub jobs_completed: Option<u64>,
    /// The main scheduler's mean cycle.
    pub main_cycle_mean_us: Option<u64>,
    /// Jobs the main scheduler looked at in its last cycle.
    pub main_queue_last: Option<u64>,
    /// The backfill scheduler's mean cycle.
    pub backfill_cycle_mean_us: Option<u64>,
    /// Jobs started by backfill since the controller started.
    pub backfilled_jobs: Option<u64>,
    /// RPCs by message type.
    pub rpcs: Vec<RpcCount>,
}

/// How often the controller served one message type, and for how long in total.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RpcCount {
    /// The message type, such as `REQUEST_JOB_INFO`.
    pub kind: String,
    /// Calls served.
    pub count: u64,
    /// Time spent serving them.
    pub total_us: u64,
}

/// One change the sampler saw in a tick.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    /// An attempt took its cores.
    Started {
        /// The attempt as the queue lists it.
        attempt: Attempt,
    },
    /// An attempt freed its cores.
    Ended {
        /// The attempt.
        key: AttemptKey,
        /// When it ended.
        at: UnixTime,
        /// How it ended, or `None` while the scheduler is still cleaning up after it.
        state: Option<EndState>,
    },
    /// How an attempt that ended with no known state finished.
    Settled {
        /// The attempt.
        key: AttemptKey,
        /// How it ended.
        state: EndState,
    },
    /// Where an attempt's cores and GPUs are, found after it started.
    Placed {
        /// The attempt.
        key: AttemptKey,
        /// Its cells.
        placement: Placement,
    },
    /// Every pending job, in queue order.
    Queue(Vec<Attempt>),
    /// Every node.
    Nodes(Vec<Node>),
    /// Every partition.
    Partitions(Vec<Partition>),
    /// Scheduler counters.
    Stats(Stats),
}

/// The changes from one tick.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Delta {
    /// Counts deltas from 1 since the sampler started.
    pub seq: u64,
    /// When the tick started.
    pub at: UnixTime,
    /// The changes, in the order they apply.
    pub events: Vec<Event>,
}

/// A running attempt and, once known, where it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Running {
    /// The attempt as the queue listed it when it started.
    pub attempt: Attempt,
    /// Its exact cells, or `None` until a lookup finds them.
    pub placement: Option<Placement>,
}

/// The cluster as a stream of deltas builds it up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cluster {
    /// When the last delta was sampled.
    pub at: Option<UnixTime>,
    /// Every node.
    pub nodes: Vec<Node>,
    /// Every partition.
    pub partitions: Vec<Partition>,
    /// Attempts that hold cores.
    pub running: BTreeMap<AttemptKey, Running>,
    /// Pending jobs, in queue order.
    pub queue: Vec<Attempt>,
    /// Scheduler counters.
    pub stats: Option<Stats>,
    /// Whether the scheduler hides other users' jobs, so cores that no visible job explains belong to someone hidden.
    pub hides_jobs: bool,
}

/// Who holds a core or GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// A visible attempt.
    Attempt(AttemptKey),
    /// A job the scheduler hides from this user.
    Hidden,
}

/// Who holds each core and GPU on one node. `None` is free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occupancy {
    /// Cores, by index.
    pub cores: Vec<Option<Owner>>,
    /// GPUs, by index.
    pub gpus: Vec<Option<Owner>>,
}

impl Cluster {
    /// Applies one delta.
    pub fn apply(&mut self, delta: &Delta) {
        self.at = Some(delta.at);
        for event in &delta.events {
            match event {
                Event::Started { attempt } => {
                    self.running.insert(
                        attempt.key,
                        Running {
                            attempt: attempt.clone(),
                            placement: None,
                        },
                    );
                }
                Event::Ended { key, .. } => {
                    self.running.remove(key);
                }
                Event::Settled { .. } => {}
                Event::Placed { key, placement } => {
                    if let Some(running) = self.running.get_mut(key) {
                        running.placement = Some(placement.clone());
                    }
                }
                Event::Queue(queue) => self.queue.clone_from(queue),
                Event::Nodes(nodes) => self.nodes.clone_from(nodes),
                Event::Partitions(partitions) => self.partitions.clone_from(partitions),
                Event::Stats(stats) => self.stats = Some(stats.clone()),
            }
        }
    }

    /// Who holds each core and GPU, node by node.
    ///
    /// Exact cells go down first. Each attempt's remaining cores then take the first free slots on its
    /// nodes, in key order: all of them when its placement is unknown, or the ones a cut-off CPU list
    /// left out. An attempt with no placement spreads its CPUs and GPUs evenly over its nodes.
    pub fn occupancy(&self) -> BTreeMap<NodeName, Occupancy> {
        let mut nodes: BTreeMap<NodeName, Occupancy> = self
            .nodes
            .iter()
            .map(|node| {
                let occupancy = Occupancy {
                    cores: vec![None; to_usize(node.cores)],
                    gpus: vec![None; to_usize(node.gpus)],
                };
                (node.name.clone(), occupancy)
            })
            .collect();
        let cpus_per_core: HashMap<&NodeName, u32> = self
            .nodes
            .iter()
            .map(|node| (&node.name, node.cpus_per_core()))
            .collect();
        for (key, running) in &self.running {
            let owner = Some(Owner::Attempt(*key));
            for cells in running.placement.iter().flat_map(|placement| &placement.0) {
                if let Some(node) = nodes.get_mut(&cells.node) {
                    claim(&mut node.cores, &cells.cores, owner);
                    claim(&mut node.gpus, &cells.gpus, owner);
                }
            }
        }
        for (key, running) in &self.running {
            let owner = Some(Owner::Attempt(*key));
            let attempt = &running.attempt;
            if let Some(placement) = &running.placement {
                for cells in &placement.0 {
                    if let Some(node) = nodes.get_mut(&cells.node) {
                        pack(&mut node.cores, cells.unplaced, owner);
                    }
                }
                continue;
            }
            let spread = u32::try_from(attempt.nodes.len())
                .unwrap_or(u32::MAX)
                .max(1);
            for name in &attempt.nodes {
                let Some(node) = nodes.get_mut(name) else {
                    continue;
                };
                let cpus = attempt.cpus.div_ceil(spread);
                let per_core = cpus_per_core.get(name).copied().unwrap_or(1);
                pack(&mut node.cores, cpus.div_ceil(per_core).max(1), owner);
                pack(&mut node.gpus, attempt.gpus.div_ceil(spread), owner);
            }
        }
        if self.hides_jobs {
            for node in &self.nodes {
                let Some(occupancy) = nodes.get_mut(&node.name) else {
                    continue;
                };
                let hidden_cores =
                    (node.cpus_alloc / node.cpus_per_core()).saturating_sub(held(&occupancy.cores));
                let hidden_gpus = node.gpus_used.saturating_sub(held(&occupancy.gpus));
                pack(&mut occupancy.cores, hidden_cores, Some(Owner::Hidden));
                pack(&mut occupancy.gpus, hidden_gpus, Some(Owner::Hidden));
            }
        }
        nodes
    }
}

fn to_usize(count: u32) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}

/// Gives `owner` the listed slots that are free. A slot past the end or already held is skipped.
fn claim(slots: &mut [Option<Owner>], indices: &[u32], owner: Option<Owner>) {
    for &index in indices {
        if let Some(slot) = slots.get_mut(to_usize(index))
            && slot.is_none()
        {
            *slot = owner;
        }
    }
}

/// Gives `owner` the first `count` free slots, or as many as are free.
fn pack(slots: &mut [Option<Owner>], count: u32, owner: Option<Owner>) {
    for slot in slots
        .iter_mut()
        .filter(|slot| slot.is_none())
        .take(to_usize(count))
    {
        *slot = owner;
    }
}

fn held(slots: &[Option<Owner>]) -> u32 {
    u32::try_from(slots.iter().filter(|slot| slot.is_some()).count()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, cores: u32, gpus: u32, cpus_alloc: u32) -> Node {
        Node {
            name: NodeName(name.into()),
            partitions: Vec::new(),
            state: NodeState::Mixed,
            flags: Vec::new(),
            state_raw: "MIXED".into(),
            reason: None,
            cpus: cores * 2,
            cpus_alloc,
            sockets: 1,
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

    fn attempt(id: u32, cpus: u32, gpus: u32, nodes: &[&str]) -> Attempt {
        Attempt {
            key: AttemptKey {
                job: JobId(id),
                submit: UnixTime(0),
            },
            array: None,
            login: Login("otter".into()),
            partitions: Vec::new(),
            state: JobState::Running,
            state_raw: "RUNNING".into(),
            cpus,
            mem_mb: None,
            gpus,
            time_limit_s: None,
            start: Some(UnixTime(1)),
            end: None,
            nodes: nodes.iter().map(|name| NodeName((*name).into())).collect(),
            reason: None,
            name: "job".into(),
        }
    }

    fn cells(node: &str, cores: &[u32], unplaced: u32, gpus: &[u32]) -> NodeCells {
        NodeCells {
            node: NodeName(node.into()),
            cores: cores.to_vec(),
            unplaced,
            gpus: gpus.to_vec(),
            mem_mb: None,
        }
    }

    fn owner(id: u32) -> Owner {
        Owner::Attempt(AttemptKey {
            job: JobId(id),
            submit: UnixTime(0),
        })
    }

    fn cluster(nodes: Vec<Node>, events: Vec<Event>) -> Cluster {
        let mut cluster = Cluster {
            nodes,
            ..Cluster::default()
        };
        cluster.apply(&Delta {
            seq: 1,
            at: UnixTime(1),
            events,
        });
        cluster
    }

    fn started(attempt: Attempt) -> Event {
        Event::Started { attempt }
    }

    fn placed(id: u32, cells: Vec<NodeCells>) -> Event {
        Event::Placed {
            key: AttemptKey {
                job: JobId(id),
                submit: UnixTime(0),
            },
            placement: Placement(cells),
        }
    }

    #[test]
    fn exact_cells_win_over_packed_ones() {
        let cluster = cluster(
            vec![node("z1", 4, 2, 0)],
            vec![
                started(attempt(1, 4, 1, &["z1"])),
                started(attempt(2, 2, 1, &["z1"])),
                placed(2, vec![cells("z1", &[0], 0, &[0])]),
            ],
        );
        let z1 = &cluster.occupancy()[&NodeName("z1".into())];
        assert_eq!(
            z1.cores,
            [Some(owner(2)), Some(owner(1)), Some(owner(1)), None]
        );
        assert_eq!(z1.gpus, [Some(owner(2)), Some(owner(1))]);
    }

    #[test]
    fn cut_off_cpu_list_packs_the_rest() {
        let cluster = cluster(
            vec![node("z1", 4, 0, 0)],
            vec![
                started(attempt(1, 6, 0, &["z1"])),
                placed(1, vec![cells("z1", &[1], 2, &[])]),
            ],
        );
        let z1 = &cluster.occupancy()[&NodeName("z1".into())];
        assert_eq!(
            z1.cores,
            [Some(owner(1)), Some(owner(1)), Some(owner(1)), None]
        );
    }

    #[test]
    fn unplaced_multi_node_job_spreads_evenly() {
        let cluster = cluster(
            vec![node("z1", 4, 2, 0), node("z2", 4, 2, 0)],
            vec![started(attempt(1, 6, 2, &["z1", "z2"]))],
        );
        for occupancy in cluster.occupancy().values() {
            assert_eq!(
                occupancy.cores,
                [Some(owner(1)), Some(owner(1)), None, None]
            );
            assert_eq!(occupancy.gpus, [Some(owner(1)), None]);
        }
    }

    #[test]
    fn ended_attempts_free_their_cells() {
        let mut cluster = cluster(
            vec![node("z1", 2, 0, 0)],
            vec![started(attempt(1, 2, 0, &["z1"]))],
        );
        cluster.apply(&Delta {
            seq: 2,
            at: UnixTime(2),
            events: vec![Event::Ended {
                key: AttemptKey {
                    job: JobId(1),
                    submit: UnixTime(0),
                },
                at: UnixTime(2),
                state: None,
            }],
        });
        assert_eq!(
            cluster.occupancy()[&NodeName("z1".into())].cores,
            [None, None]
        );
    }

    #[test]
    fn hidden_jobs_fill_cores_no_visible_job_explains() {
        let mut nodes = vec![node("z1", 4, 0, 6)];
        let mut cluster = cluster(nodes.clone(), vec![started(attempt(1, 2, 0, &["z1"]))]);
        let hidden = Some(Owner::Hidden);
        assert_eq!(
            cluster.occupancy()[&NodeName("z1".into())].cores,
            [Some(owner(1)), None, None, None],
            "sinfo and squeue can disagree when every job is visible"
        );
        cluster.hides_jobs = true;
        assert_eq!(
            cluster.occupancy()[&NodeName("z1".into())].cores,
            [Some(owner(1)), hidden, hidden, None]
        );
        nodes[0].cpus_alloc = 0;
        cluster.nodes = nodes;
        assert_eq!(
            cluster.occupancy()[&NodeName("z1".into())].cores,
            [Some(owner(1)), None, None, None]
        );
    }
}
