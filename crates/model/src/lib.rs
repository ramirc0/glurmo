//! Scheduler-neutral types that the Slurm module returns and the views consume.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub mod jobs;
pub mod layout;
pub mod summary;
pub mod theme;
pub mod units;
pub mod usage;

/// A job ID unique to each array task, as sacct's `JobIDRaw` prints it.
///
/// # Examples
///
/// ```
/// use glurmo_model::JobId;
///
/// assert_eq!(serde_json::to_string(&JobId(4242)).unwrap(), "4242");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub u32);

/// Seconds since the Unix epoch.
///
/// # Examples
///
/// ```
/// use glurmo_model::UnixTime;
///
/// assert!(UnixTime(10) < UnixTime(20));
/// assert_eq!(serde_json::to_string(&UnixTime(10)).unwrap(), "10");
/// ```
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct UnixTime(pub i64);

impl UnixTime {
    /// Reads the system clock. A clock set before 1970 reads as 0.
    ///
    /// It does not exist on wasm, where the standard library's clock panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::UnixTime;
    ///
    /// assert!(UnixTime::now() > UnixTime(1_700_000_000));
    /// ```
    #[cfg(not(target_family = "wasm"))]
    pub fn now() -> Self {
        let since = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self(i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
    }
}

/// One run of a job. A requeue keeps the job ID but resets the submit time.
///
/// # Examples
///
/// ```
/// use glurmo_model::{AttemptKey, JobId, UnixTime};
///
/// let first = AttemptKey { job: JobId(7), submit: UnixTime(100) };
/// let requeued = AttemptKey { job: JobId(7), submit: UnixTime(500) };
/// assert_ne!(first, requeued);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AttemptKey {
    /// The job ID.
    pub job: JobId,
    /// When this attempt was submitted.
    pub submit: UnixTime,
}

/// A user's login name.
///
/// # Examples
///
/// ```
/// use glurmo_model::Login;
///
/// let login: Login = serde_json::from_str(r#""otter""#).unwrap();
/// assert_eq!(login, Login("otter".into()));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Login(pub String);

/// A compute node's hostname.
///
/// # Examples
///
/// ```
/// use glurmo_model::NodeName;
///
/// assert_eq!(serde_json::to_string(&NodeName("z1001".into())).unwrap(), r#""z1001""#);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeName(pub String);

/// A partition, or a queue in schedulers that call it that.
///
/// # Examples
///
/// ```
/// use glurmo_model::PartitionName;
///
/// assert_eq!(serde_json::to_string(&PartitionName("5days".into())).unwrap(), r#""5days""#);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PartitionName(pub String);

/// Where a job is in its life.
///
/// # Examples
///
/// ```
/// use glurmo_model::{EndState, JobState};
///
/// let state: JobState = serde_json::from_str(r#"{"ended": "timeout"}"#).unwrap();
/// assert_eq!(state, JobState::Ended(EndState::Timeout));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// # Examples
///
/// ```
/// use glurmo_model::EndState;
///
/// let state: EndState = serde_json::from_str(r#""out_of_memory""#).unwrap();
/// assert_eq!(state, EndState::OutOfMemory);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl EndState {
    /// Whether the attempt failed to finish its work: it failed, ran out of memory or time, or lost its node.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::EndState;
    ///
    /// assert!(EndState::Timeout.is_failure());
    /// assert!(!EndState::Cancelled.is_failure());
    /// ```
    pub fn is_failure(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::OutOfMemory | Self::Timeout | Self::NodeFail
        )
    }
}

/// The tasks of a job array that one queue row stands for.
///
/// # Examples
///
/// ```
/// use glurmo_model::{ArrayTasks, JobId};
///
/// let tasks = ArrayTasks { job: JobId(3441951), spec: "0-3,8%2".into(), count: 5 };
/// assert_eq!(tasks.count, 5);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArrayTasks {
    /// The array's base job ID.
    pub job: JobId,
    /// Task IDs as the scheduler prints them, such as `174` or `0-3,8%2`.
    pub spec: String,
    /// How many tasks the spec covers.
    pub count: u32,
}

/// A job as the queue lists it.
///
/// # Examples
///
/// ```
/// # let json = r#"{
/// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
/// #     "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
/// #     "mem_mb": 16000, "gpus": 1, "time_limit_s": 86400, "start": 1700000100, "end": null,
/// #     "nodes": ["z1001"], "reason": null, "name": "align"
/// # }"#;
/// use glurmo_model::{Attempt, JobState};
///
/// let attempt: Attempt = serde_json::from_str(json).unwrap();
/// assert_eq!(attempt.state, JobState::Running);
/// assert_eq!(attempt.login.0, "otter");
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

impl Attempt {
    /// How many tasks the row stands for: an array range's count, or 1.
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
    /// use glurmo_model::Attempt;
    ///
    /// let mut attempt: Attempt = serde_json::from_str(json).unwrap();
    /// assert_eq!(attempt.tasks(), 4);
    /// attempt.array = None;
    /// assert_eq!(attempt.tasks(), 1);
    /// ```
    pub fn tasks(&self) -> u32 {
        self.array.as_ref().map_or(1, |array| array.count)
    }

    /// The job ID as the scheduler prints it: `JOB_TASKS` for an array, else the job ID.
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "key": {"job": 4250, "submit": 1700000000}, "array": {"job": 4242, "spec": "8", "count": 1},
    /// #     "login": "otter", "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
    /// #     "mem_mb": null, "gpus": 0, "time_limit_s": null, "start": null, "end": null,
    /// #     "nodes": [], "reason": null, "name": "align"
    /// # }"#;
    /// use glurmo_model::Attempt;
    ///
    /// let mut attempt: Attempt = serde_json::from_str(json).unwrap();
    /// assert_eq!(attempt.printed_id(), "4242_8");
    /// attempt.array = None;
    /// assert_eq!(attempt.printed_id(), "4250");
    /// ```
    pub fn printed_id(&self) -> String {
        match &self.array {
            Some(array) => format!("{}_{}", array.job.0, array.spec),
            None => self.key.job.0.to_string(),
        }
    }
}

/// A node's state, neutral across schedulers.
///
/// # Examples
///
/// ```
/// use glurmo_model::NodeState;
///
/// assert_eq!(serde_json::to_string(&NodeState::Draining).unwrap(), r#""draining""#);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// # Examples
///
/// ```
/// use glurmo_model::NodeFlag;
///
/// let flag: NodeFlag = serde_json::from_str(r#""not_responding""#).unwrap();
/// assert_eq!(flag, NodeFlag::NotResponding);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// # Examples
///
/// ```
/// # let json = r#"{
/// #     "name": "z1001", "partitions": ["5days"], "state": "mixed", "flags": [], "state_raw": "MIXED",
/// #     "reason": null, "cpus": 8, "cpus_alloc": 4, "sockets": 1, "cores": 4, "load": 2.5,
/// #     "mem_mb": 64000, "mem_alloc_mb": 16000, "mem_free_mb": 40000, "gpus": 2, "gpus_used": 1,
/// #     "gpu_model": "V100", "features": ["avx512"]
/// # }"#;
/// use glurmo_model::{Node, NodeState};
///
/// let node: Node = serde_json::from_str(json).unwrap();
/// assert_eq!(node.state, NodeState::Mixed);
/// assert_eq!(node.gpu_model.as_deref(), Some("V100"));
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "name": "z1001", "partitions": ["5days"], "state": "mixed", "flags": [], "state_raw": "MIXED",
    /// #     "reason": null, "cpus": 8, "cpus_alloc": 4, "sockets": 1, "cores": 4, "load": 2.5,
    /// #     "mem_mb": 64000, "mem_alloc_mb": 16000, "mem_free_mb": 40000, "gpus": 2, "gpus_used": 1,
    /// #     "gpu_model": "V100", "features": ["avx512"]
    /// # }"#;
    /// use glurmo_model::Node;
    ///
    /// let mut node: Node = serde_json::from_str(json).unwrap();
    /// assert_eq!(node.cpus_per_core(), 2);
    /// node.cpus = 4;
    /// assert_eq!(node.cpus_per_core(), 1);
    /// ```
    pub fn cpus_per_core(&self) -> u32 {
        self.cpus.checked_div(self.cores).unwrap_or(1).max(1)
    }

    /// Whether the node can run jobs: up, responding, and not drained.
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "name": "z1001", "partitions": ["5days"], "state": "mixed", "flags": [], "state_raw": "MIXED",
    /// #     "reason": null, "cpus": 8, "cpus_alloc": 4, "sockets": 1, "cores": 4, "load": 2.5,
    /// #     "mem_mb": 64000, "mem_alloc_mb": 16000, "mem_free_mb": 40000, "gpus": 2, "gpus_used": 1,
    /// #     "gpu_model": "V100", "features": ["avx512"]
    /// # }"#;
    /// use glurmo_model::{Node, NodeState};
    ///
    /// let mut node: Node = serde_json::from_str(json).unwrap();
    /// assert!(node.is_usable());
    /// node.state = NodeState::Drained;
    /// assert!(!node.is_usable());
    /// ```
    pub fn is_usable(&self) -> bool {
        !matches!(
            self.state,
            NodeState::Down | NodeState::Drained | NodeState::Unknown
        ) && !self.flags.contains(&NodeFlag::NotResponding)
    }

    /// A short word for why the node cannot take new jobs, or `None` when it can.
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "name": "z1001", "partitions": ["5days"], "state": "mixed", "flags": [], "state_raw": "MIXED",
    /// #     "reason": null, "cpus": 8, "cpus_alloc": 4, "sockets": 1, "cores": 4, "load": 2.5,
    /// #     "mem_mb": 64000, "mem_alloc_mb": 16000, "mem_free_mb": 40000, "gpus": 2, "gpus_used": 1,
    /// #     "gpu_model": "V100", "features": ["avx512"]
    /// # }"#;
    /// use glurmo_model::{Node, NodeFlag, NodeState};
    ///
    /// let mut node: Node = serde_json::from_str(json).unwrap();
    /// assert_eq!(node.trouble(), None);
    /// node.state = NodeState::Draining;
    /// assert_eq!(node.trouble(), Some("draining"));
    /// node.flags.push(NodeFlag::NotResponding);
    /// assert_eq!(node.trouble(), Some("no reply"));
    /// ```
    pub fn trouble(&self) -> Option<&'static str> {
        if self.flags.contains(&NodeFlag::NotResponding) {
            return Some("no reply");
        }
        match self.state {
            NodeState::Down => Some("down"),
            NodeState::Drained => Some("drained"),
            NodeState::Draining => Some("draining"),
            NodeState::Unknown => Some("unknown"),
            _ => None,
        }
    }

    /// Whether the scheduler may start new jobs on the node. A draining node still runs its jobs but
    /// takes no new ones, so sinfo counts its idle CPUs as "other".
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "name": "z1001", "partitions": ["5days"], "state": "mixed", "flags": [], "state_raw": "MIXED",
    /// #     "reason": null, "cpus": 8, "cpus_alloc": 4, "sockets": 1, "cores": 4, "load": 2.5,
    /// #     "mem_mb": 64000, "mem_alloc_mb": 16000, "mem_free_mb": 40000, "gpus": 2, "gpus_used": 1,
    /// #     "gpu_model": "V100", "features": ["avx512"]
    /// # }"#;
    /// use glurmo_model::{Node, NodeState};
    ///
    /// let mut node: Node = serde_json::from_str(json).unwrap();
    /// assert!(node.takes_new_jobs());
    /// node.state = NodeState::Draining;
    /// assert!(node.is_usable());
    /// assert!(!node.takes_new_jobs());
    /// ```
    pub fn takes_new_jobs(&self) -> bool {
        self.trouble().is_none()
    }
}

/// A partition and its nodes.
///
/// # Examples
///
/// ```
/// use glurmo_model::Partition;
///
/// let json = r#"{"name": "5days", "nodes": ["z1001"], "max_time_s": 432000,
///                "default": false, "hidden": false, "state_raw": "UP"}"#;
/// let partition: Partition = serde_json::from_str(json).unwrap();
/// assert_eq!(partition.max_time_s, Some(432_000));
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
///
/// # Examples
///
/// ```
/// use glurmo_model::{NodeCells, NodeName};
///
/// let cells = NodeCells {
///     node: NodeName("z1001".into()),
///     cores: vec![0, 1],
///     unplaced: 0,
///     gpus: vec![1],
///     mem_mb: Some(16_000),
/// };
/// assert_eq!(cells.cores.len(), 2);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// # Examples
///
/// ```
/// use glurmo_model::Placement;
///
/// let placement: Placement = serde_json::from_str(
///     r#"[{"node": "z1001", "cores": [0, 1], "unplaced": 0, "gpus": [], "mem_mb": null}]"#,
/// )
/// .unwrap();
/// assert_eq!(placement.0[0].node.0, "z1001");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Placement(pub Vec<NodeCells>);

impl Placement {
    /// Whether every core's index is known.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::{NodeCells, NodeName, Placement};
    ///
    /// let mut cells = NodeCells { node: NodeName("z1".into()), cores: vec![0], unplaced: 0, gpus: vec![], mem_mb: None };
    /// assert!(Placement(vec![cells.clone()]).is_exact());
    /// cells.unplaced = 3;
    /// assert!(!Placement(vec![cells]).is_exact());
    /// ```
    pub fn is_exact(&self) -> bool {
        self.0.iter().all(|cells| cells.unplaced == 0)
    }
}

/// Counters since the controller's last daily reset. A counter this Slurm version does not print is `None`.
///
/// # Examples
///
/// ```
/// use glurmo_model::Stats;
///
/// let stats = Stats { jobs_started: Some(1_200), ..Stats::default() };
/// assert_eq!(stats.jobs_completed, None);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// # Examples
///
/// ```
/// use glurmo_model::RpcCount;
///
/// let rpc = RpcCount { kind: "REQUEST_JOB_INFO".into(), count: 9, total_us: 270_000 };
/// assert_eq!(rpc.total_us / rpc.count, 30_000);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcCount {
    /// The message type, such as `REQUEST_JOB_INFO`.
    pub kind: String,
    /// Calls served.
    pub count: u64,
    /// Time spent serving them.
    pub total_us: u64,
}

/// Someone who has held cores or waited in the queue.
///
/// # Examples
///
/// ```
/// use glurmo_model::{Login, User};
///
/// let user: User = serde_json::from_str(r#"{"login": "otter", "name": null, "color_slot": 3}"#).unwrap();
/// assert_eq!(user.login, Login("otter".into()));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// The login.
    pub login: Login,
    /// The full name, when the config asks for full names and the system knows one.
    pub name: Option<String>,
    /// A palette slot, assigned in order of first sight and kept for good.
    pub color_slot: u32,
}

/// A stretch of failed ticks, which leaves the view stale. It usually means the controller is down.
///
/// # Examples
///
/// ```
/// use glurmo_model::{Outage, UnixTime};
///
/// let outage = Outage { since: UnixTime(1_700_000_000), error: "squeue timed out".into() };
/// assert_eq!(outage.since, UnixTime(1_700_000_000));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outage {
    /// When the first failed tick started.
    pub since: UnixTime,
    /// Why that tick failed.
    pub error: String,
}

/// One change the sampler saw in a tick.
///
/// # Examples
///
/// ```
/// use glurmo_model::{AttemptKey, Event, JobId, UnixTime};
///
/// let event: Event = serde_json::from_str(
///     r#"{"ended": {"key": {"job": 7, "submit": 100}, "at": 200, "state": null}}"#,
/// )
/// .unwrap();
/// let key = AttemptKey { job: JobId(7), submit: UnixTime(100) };
/// assert_eq!(event, Event::Ended { key, at: UnixTime(200), state: None });
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// A pending attempt left the queue without taking cores, usually because it was cancelled.
    Withdrawn {
        /// The attempt as the queue last listed it, with its end and final state.
        attempt: Attempt,
    },
    /// How an attempt that ended with no known state finished.
    Settled {
        /// The attempt.
        key: AttemptKey,
        /// How it ended.
        state: EndState,
    },
    /// A running attempt was suspended or resumed, or its time limit changed.
    Updated {
        /// The attempt as the listing now shows it.
        attempt: Attempt,
    },
    /// Where an attempt's cores and GPUs are, found after it started.
    Placed {
        /// The attempt.
        key: AttemptKey,
        /// Its cells.
        placement: Placement,
    },
    /// Pending jobs that joined the queue or changed, in listing order.
    Queued(Vec<Attempt>),
    /// Pending jobs that left the queue, because they started or were withdrawn.
    Dequeued(Vec<AttemptKey>),
    /// Every node.
    Nodes(Vec<Node>),
    /// Nodes whose records changed, each in place of its old record. The nodes and their order stay as
    /// the last [`Event::Nodes`] gave them.
    NodesChanged(Vec<Node>),
    /// Every partition.
    Partitions(Vec<Partition>),
    /// Users seen for the first time.
    Users(Vec<User>),
    /// Ticks started failing, or work again when `None`.
    Outage(Option<Outage>),
}

/// The changes from one tick.
///
/// # Examples
///
/// ```
/// use glurmo_model::{Delta, Event};
///
/// let delta: Delta = serde_json::from_str(r#"{"seq": 3, "at": 200, "events": [{"outage": null}]}"#).unwrap();
/// assert_eq!(delta.events, [Event::Outage(None)]);
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    /// Counts deltas from 1 since the sampler started.
    pub seq: u64,
    /// When the tick started.
    pub at: UnixTime,
    /// The changes, in the order they apply.
    pub events: Vec<Event>,
}

/// A running attempt and, once known, where it runs.
///
/// # Examples
///
/// ```
/// # let json = r#"{
/// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
/// #     "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
/// #     "mem_mb": 16000, "gpus": 1, "time_limit_s": 86400, "start": 1700000100, "end": null,
/// #     "nodes": ["z1001"], "reason": null, "name": "align"
/// # }"#;
/// use glurmo_model::{Attempt, Running};
///
/// let running = Running { attempt: serde_json::from_str(json).unwrap(), placement: None };
/// assert!(running.placement.is_none());
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Running {
    /// The attempt as the queue listed it when it started.
    pub attempt: Attempt,
    /// Its exact cells, or `None` until a lookup finds them.
    pub placement: Option<Placement>,
}

/// The cluster as a stream of deltas builds it up.
///
/// # Examples
///
/// ```
/// use glurmo_model::Cluster;
///
/// let cluster: Cluster = serde_json::from_str(
///     r#"{"at": null, "nodes": [], "partitions": [], "running": [], "queue": [],
///         "hides_jobs": false, "users": {}, "outage": null}"#,
/// )
/// .unwrap();
/// assert!(cluster.running.is_empty());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    /// When the last delta was sampled.
    pub at: Option<UnixTime>,
    /// Every node.
    pub nodes: Vec<Node>,
    /// Every partition.
    pub partitions: Vec<Partition>,
    /// Attempts that hold cores.
    #[serde(serialize_with = "values", deserialize_with = "keyed")]
    pub running: BTreeMap<AttemptKey, Running>,
    /// Pending jobs.
    #[serde(serialize_with = "values", deserialize_with = "keyed")]
    pub queue: BTreeMap<AttemptKey, Attempt>,
    /// Whether the scheduler hides other users' jobs, so cores that no visible job explains belong to someone hidden.
    pub hides_jobs: bool,
    /// Everyone seen so far.
    pub users: BTreeMap<Login, User>,
    /// Whether ticks are failing, and since when.
    pub outage: Option<Outage>,
}

/// Writes a map keyed by attempt as a list of its values, because JSON keys must be strings.
fn values<V: Serialize, S: Serializer>(
    map: &BTreeMap<AttemptKey, V>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(map.values())
}

/// Something stored under its attempt's key.
trait Keyed {
    fn key(&self) -> AttemptKey;
}

impl Keyed for Attempt {
    fn key(&self) -> AttemptKey {
        self.key
    }
}

impl Keyed for Running {
    fn key(&self) -> AttemptKey {
        self.attempt.key
    }
}

/// Reads a list that [`values`] wrote back into a map keyed by attempt.
fn keyed<'de, V: Deserialize<'de> + Keyed, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<AttemptKey, V>, D::Error> {
    let list = Vec::<V>::deserialize(deserializer)?;
    Ok(list.into_iter().map(|value| (value.key(), value)).collect())
}

/// Who holds a core or GPU.
///
/// # Examples
///
/// ```
/// use glurmo_model::{AttemptKey, JobId, Owner, UnixTime};
///
/// let owner = Owner::Attempt(AttemptKey { job: JobId(7), submit: UnixTime(100) });
/// assert_ne!(owner, Owner::Hidden);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    /// A visible attempt.
    Attempt(AttemptKey),
    /// A job the scheduler hides from this user.
    Hidden,
}

/// Who holds each core and GPU on one node. `None` is free.
///
/// # Examples
///
/// ```
/// use glurmo_model::{Occupancy, Owner};
///
/// let occupancy = Occupancy { cores: vec![Some(Owner::Hidden), None], gpus: vec![] };
/// assert_eq!(occupancy.cores.iter().flatten().count(), 1);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occupancy {
    /// Cores, by index.
    pub cores: Vec<Option<Owner>>,
    /// GPUs, by index.
    pub gpus: Vec<Option<Owner>>,
}

/// Who holds each core and GPU, node by node.
///
/// # Examples
///
/// ```
/// use glurmo_model::{Cluster, Occupancies};
///
/// let occupancy: Occupancies = Cluster::default().occupancy();
/// assert!(occupancy.is_empty());
/// ```
pub type Occupancies = BTreeMap<NodeName, Occupancy>;

impl Cluster {
    /// Applies one delta.
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
    /// #     "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
    /// #     "mem_mb": 16000, "gpus": 1, "time_limit_s": 86400, "start": 1700000100, "end": null,
    /// #     "nodes": ["z1001"], "reason": null, "name": "align"
    /// # }"#;
    /// use glurmo_model::{Cluster, Delta, Event};
    ///
    /// let mut cluster = Cluster::default();
    /// let attempt = serde_json::from_str(json).unwrap();
    /// cluster.apply(&Delta { seq: 1, at: glurmo_model::UnixTime(200), events: vec![Event::Started { attempt }] });
    /// assert_eq!(cluster.running.len(), 1);
    /// ```
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
                Event::Withdrawn { .. } | Event::Settled { .. } => {}
                Event::Updated { attempt } => {
                    if let Some(running) = self.running.get_mut(&attempt.key) {
                        running.attempt = attempt.clone();
                    }
                }
                Event::Placed { key, placement } => {
                    if let Some(running) = self.running.get_mut(key) {
                        running.placement = Some(placement.clone());
                    }
                }
                Event::Queued(attempts) => {
                    for attempt in attempts {
                        self.queue.insert(attempt.key, attempt.clone());
                    }
                }
                Event::Dequeued(keys) => {
                    for key in keys {
                        self.queue.remove(key);
                    }
                }
                Event::Nodes(nodes) => self.nodes.clone_from(nodes),
                Event::NodesChanged(changed) => {
                    let changed: BTreeMap<&NodeName, &Node> =
                        changed.iter().map(|node| (&node.name, node)).collect();
                    for node in &mut self.nodes {
                        if let Some(&new) = changed.get(&node.name) {
                            node.clone_from(new);
                        }
                    }
                }
                Event::Partitions(partitions) => self.partitions.clone_from(partitions),
                Event::Users(users) => {
                    for user in users {
                        self.users.insert(user.login.clone(), user.clone());
                    }
                }
                Event::Outage(outage) => self.outage.clone_from(outage),
            }
        }
    }

    /// Who holds each core and GPU, node by node.
    ///
    /// Exact cells go down first. Each attempt's remaining cores then take the first free slots on its
    /// nodes, in key order: all of them when its placement is unknown, or the ones a cut-off CPU list
    /// left out. An attempt with no placement spreads its CPUs and GPUs evenly over its nodes.
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
    /// #     "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
    /// #     "mem_mb": 16000, "gpus": 1, "time_limit_s": 86400, "start": 1700000100, "end": null,
    /// #     "nodes": ["z1001"], "reason": null, "name": "align"
    /// # }"#;
    /// # let node = r#"{
    /// #     "name": "z1001", "partitions": ["5days"], "state": "mixed", "flags": [], "state_raw": "MIXED",
    /// #     "reason": null, "cpus": 8, "cpus_alloc": 4, "sockets": 1, "cores": 4, "load": 2.5,
    /// #     "mem_mb": 64000, "mem_alloc_mb": 16000, "mem_free_mb": 40000, "gpus": 2, "gpus_used": 1,
    /// #     "gpu_model": "V100", "features": ["avx512"]
    /// # }"#;
    /// use glurmo_model::{Cluster, Delta, Event, NodeName, UnixTime};
    ///
    /// let mut cluster = Cluster { nodes: vec![serde_json::from_str(node).unwrap()], ..Cluster::default() };
    /// let attempt = serde_json::from_str(json).unwrap();
    /// cluster.apply(&Delta { seq: 1, at: UnixTime(200), events: vec![Event::Started { attempt }] });
    /// let z1001 = &cluster.occupancy()[&NodeName("z1001".into())];
    /// assert_eq!(z1001.cores.iter().flatten().count(), 4, "8 CPUs at 2 threads per core");
    /// assert_eq!(z1001.gpus.iter().flatten().count(), 1);
    /// ```
    pub fn occupancy(&self) -> Occupancies {
        let mut nodes: Occupancies = self
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
            let share = |total: u32, i: u32| total / spread + u32::from(i < total % spread);
            for (i, name) in (0..).zip(&attempt.nodes) {
                let Some(node) = nodes.get_mut(name) else {
                    continue;
                };
                let per_core = cpus_per_core.get(name).copied().unwrap_or(1);
                let cores = share(attempt.cpus, i).div_ceil(per_core).max(1);
                pack(&mut node.cores, cores, owner);
                pack(&mut node.gpus, share(attempt.gpus, i), owner);
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

    /// Sets each node's CPUs, GPUs and memory allocated from the attempts running on it, and an up node's
    /// state to idle, mixed or allocated to match. A node that is draining, drained, down or unknown keeps
    /// its state.
    ///
    /// History keeps no allocation, so a cluster rebuilt from it needs this after every change. Hidden
    /// jobs come from the allocation, so it is for clusters that hide no jobs.
    ///
    /// # Examples
    ///
    /// ```
    /// # let json = r#"{
    /// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
    /// #     "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
    /// #     "mem_mb": 16000, "gpus": 1, "time_limit_s": 86400, "start": 1700000100, "end": null,
    /// #     "nodes": ["z1001"], "reason": null, "name": "align"
    /// # }"#;
    /// # let node = r#"{
    /// #     "name": "z1001", "partitions": ["5days"], "state": "idle", "flags": [], "state_raw": "IDLE",
    /// #     "reason": null, "cpus": 8, "cpus_alloc": 0, "sockets": 1, "cores": 4, "load": null,
    /// #     "mem_mb": 64000, "mem_alloc_mb": 0, "mem_free_mb": null, "gpus": 2, "gpus_used": 0,
    /// #     "gpu_model": "V100", "features": []
    /// # }"#;
    /// use glurmo_model::{Cluster, Delta, Event, NodeState, UnixTime};
    ///
    /// let mut cluster = Cluster { nodes: vec![serde_json::from_str(node).unwrap()], ..Cluster::default() };
    /// let attempt = serde_json::from_str(json).unwrap();
    /// cluster.apply(&Delta { seq: 1, at: UnixTime(200), events: vec![Event::Started { attempt }] });
    /// cluster.tally_allocation();
    /// assert_eq!(cluster.nodes[0].state, NodeState::Allocated, "8 CPUs fill 4 cores");
    /// assert_eq!((cluster.nodes[0].cpus_alloc, cluster.nodes[0].gpus_used), (8, 1));
    /// assert_eq!(cluster.nodes[0].mem_alloc_mb, 16000);
    /// ```
    pub fn tally_allocation(&mut self) {
        let occupancy = self.occupancy();
        let mem_held = self.mem_held();
        for node in &mut self.nodes {
            node.mem_alloc_mb = mem_held.get(&node.name).copied().unwrap_or(0);
            let Some(cells) = occupancy.get(&node.name) else {
                continue;
            };
            let cores = held(&cells.cores);
            node.cpus_alloc = cores * node.cpus_per_core();
            node.gpus_used = held(&cells.gpus);
            if !matches!(
                node.state,
                NodeState::Idle | NodeState::Mixed | NodeState::Allocated
            ) {
                continue;
            }
            (node.state, node.state_raw) = match cores {
                0 => (NodeState::Idle, "IDLE".to_owned()),
                held if held >= node.cores => (NodeState::Allocated, "ALLOCATED".to_owned()),
                _ => (NodeState::Mixed, "MIXED".to_owned()),
            };
        }
    }

    /// Memory each node holds for running attempts: what a lookup found on the node, or else the
    /// attempt's memory split evenly across its nodes.
    fn mem_held(&self) -> HashMap<NodeName, u64> {
        let mut mem_held: HashMap<NodeName, u64> = HashMap::new();
        for running in self.running.values() {
            let attempt = &running.attempt;
            let spread = u64::try_from(attempt.nodes.len())
                .unwrap_or(u64::MAX)
                .max(1);
            let even = attempt.mem_mb.map_or(0, |mb| mb / spread);
            match &running.placement {
                Some(placement) => {
                    for cells in &placement.0 {
                        *mem_held.entry(cells.node.clone()).or_default() +=
                            cells.mem_mb.unwrap_or(even);
                    }
                }
                None => {
                    for node in &attempt.nodes {
                        *mem_held.entry(node.clone()).or_default() += even;
                    }
                }
            }
        }
        mem_held
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
    fn an_update_replaces_a_running_attempt_and_keeps_its_placement() {
        let mut suspended = attempt(1, 4, 0, &["z1"]);
        suspended.state = JobState::Suspended;
        let cluster = cluster(
            vec![node("z1", 4, 0, 0)],
            vec![
                started(attempt(1, 4, 0, &["z1"])),
                placed(1, vec![cells("z1", &[0, 1], 0, &[])]),
                Event::Updated {
                    attempt: suspended.clone(),
                },
            ],
        );
        let running = &cluster.running[&suspended.key];
        assert_eq!(running.attempt, suspended);
        assert!(running.placement.is_some());
    }

    #[test]
    fn changed_nodes_replace_their_records_in_place() {
        let mut busy = node("z2", 4, 0, 4);
        busy.load = Some(3.9);
        let cluster = cluster(
            Vec::new(),
            vec![
                Event::Nodes(vec![node("z1", 4, 0, 0), node("z2", 4, 0, 0)]),
                Event::NodesChanged(vec![busy.clone()]),
            ],
        );
        assert_eq!(cluster.nodes, [node("z1", 4, 0, 0), busy]);
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
    fn unplaced_job_spreads_what_does_not_divide_evenly_over_the_first_nodes() {
        let cluster = cluster(
            vec![node("z1", 4, 2, 0), node("z2", 4, 2, 0)],
            vec![started(attempt(1, 5, 1, &["z1", "z2"]))],
        );
        let occupancy = cluster.occupancy();
        let held = |cells: fn(&Occupancy) -> &Vec<Option<Owner>>| -> usize {
            occupancy
                .values()
                .flat_map(cells)
                .filter(|cell| cell.is_some())
                .count()
        };
        assert_eq!(held(|occupancy| &occupancy.cores), 3);
        assert_eq!(held(|occupancy| &occupancy.gpus), 1);
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
