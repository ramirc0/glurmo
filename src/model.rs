//! Scheduler-neutral types that the Slurm module returns and the views consume.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A job ID unique to each array task, as sacct's `JobIDRaw` prints it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct JobId(pub u32);

/// Seconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
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
