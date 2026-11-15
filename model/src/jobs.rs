//! The job list: running and pending attempts, narrowed by a search, grouped, and ordered.
//!
//! Attempts from one user that share a phase, a name or array, and partitions form one group, so a
//! workflow's thousand pending jobs take one row.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    ArrayTasks, Attempt, AttemptKey, Cluster, JobId, JobState, Login, NodeName, PartitionName,
    Running, UnixTime,
};

/// Whether attempts hold cores yet.
///
/// # Examples
///
/// ```
/// use glurmo_model::jobs::Phase;
///
/// assert!(Phase::Running < Phase::Pending, "running groups list first");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Phase {
    /// Holding cores, or suspended on them.
    Running,
    /// Waiting in the queue.
    Pending,
}

/// What the list shows.
///
/// # Examples
///
/// ```
/// use glurmo_model::jobs::{Phase, Search};
///
/// let search = Search { text: "bash long", phase: Some(Phase::Running), ..Search::default() };
/// assert!(search.login.is_none() && !search.gpus);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Search<'a> {
    /// Words that must each appear in a job's login, full name, ID, name, partitions, nodes or reason,
    /// without regard to case.
    pub text: &'a str,
    /// Only this phase, or both.
    pub phase: Option<Phase>,
    /// Only this user's jobs, or everyone's.
    pub login: Option<&'a Login>,
    /// Only jobs that hold or ask for GPUs.
    pub gpus: bool,
}

/// How groups are ordered within each phase. Running groups always come before pending ones.
///
/// # Examples
///
/// ```
/// use glurmo_model::jobs::Order;
///
/// assert_eq!(Order::default(), Order::Cpus);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    /// Most CPUs first.
    #[default]
    Cpus,
    /// Most GPUs first.
    Gpus,
    /// Most memory first.
    Memory,
    /// Longest running, or longest waiting, first.
    Time,
    /// By login.
    User,
}

/// What attempts share to fall in one group.
///
/// # Examples
///
/// ```
/// use glurmo_model::Login;
/// use glurmo_model::jobs::{GroupKey, Phase};
///
/// let key = GroupKey { phase: Phase::Pending, login: Login("otter".into()), family: "align".into(), partitions: "5days".into() };
/// assert_eq!(key.clone(), key);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupKey {
    /// Running or pending.
    pub phase: Phase,
    /// The owner.
    pub login: Login,
    /// The array's base job ID, or else the job name.
    pub family: String,
    /// The partitions, comma-separated.
    pub partitions: String,
}

impl GroupKey {
    fn of(attempt: &Attempt, phase: Phase) -> Self {
        Self {
            phase,
            login: attempt.login.clone(),
            family: attempt
                .array
                .as_ref()
                .map_or_else(|| attempt.name.clone(), |array| array.job.0.to_string()),
            partitions: partitions(attempt),
        }
    }
}

/// Attempts that share a [`GroupKey`], by job ID.
///
/// # Examples
///
/// ```
/// use glurmo_model::Cluster;
/// use glurmo_model::jobs::{Order, Search, job_groups};
///
/// assert!(job_groups(&Cluster::default(), &Search::default(), Order::default()).is_empty());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct JobGroup<'a> {
    /// What they share.
    pub key: GroupKey,
    /// The attempts, by job ID.
    pub attempts: Vec<&'a Attempt>,
}

impl<'a> JobGroup<'a> {
    /// The first attempt, which speaks for the group's name and partitions.
    ///
    /// # Panics
    ///
    /// Never for a group from [`job_groups`], which has at least one attempt.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!(groups[0].first().name, "align");
    /// ```
    pub fn first(&self) -> &'a Attempt {
        self.attempts[0]
    }

    /// Tasks, counting each task of a pending array range.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!((groups[0].tasks(), groups[1].tasks()), (2, 4));
    /// ```
    pub fn tasks(&self) -> u64 {
        self.sum(|_| 1)
    }

    /// CPUs held or asked for.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!((groups[0].cpus(), groups[1].cpus()), (16, 4));
    /// ```
    pub fn cpus(&self) -> u64 {
        self.sum(|attempt| u64::from(attempt.cpus))
    }

    /// GPUs held or asked for.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!(groups[0].gpus(), 2);
    /// ```
    pub fn gpus(&self) -> u64 {
        self.sum(|attempt| u64::from(attempt.gpus))
    }

    /// Memory held or asked for, or `None` when no attempt says.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!((groups[0].mem_mb(), groups[1].mem_mb()), (Some(32_000), None));
    /// ```
    pub fn mem_mb(&self) -> Option<u64> {
        self.attempts
            .iter()
            .any(|attempt| attempt.mem_mb.is_some())
            .then(|| self.sum(|attempt| attempt.mem_mb.unwrap_or(0)))
    }

    /// When the earliest attempt started running, or was submitted while pending.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::UnixTime;
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!(groups[0].since(), Some(UnixTime(1_700_000_100)));
    /// assert_eq!(groups[1].since(), Some(UnixTime(1_700_000_000)));
    /// ```
    pub fn since(&self) -> Option<UnixTime> {
        self.attempts
            .iter()
            .filter_map(|attempt| match self.key.phase {
                Phase::Running => attempt.start,
                Phase::Pending => Some(attempt.key.submit),
            })
            .min()
    }

    /// The job ID as the scheduler prints it for one attempt, the array as `BASE_*`, or else the first
    /// and last job IDs.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!(groups[0].label(), "4250…4251");
    /// assert_eq!(groups[1].label(), "4242_0-3");
    /// ```
    pub fn label(&self) -> String {
        let first = self.first();
        if self.attempts.len() == 1 {
            return first.printed_id();
        }
        if let Some(array) = &first.array {
            return format!("{}_*", array.job.0);
        }
        let last = self.attempts[self.attempts.len() - 1];
        format!("{}…{}", first.key.job.0, last.key.job.0)
    }

    /// Where a running group runs, or why a pending group waits. A group of more than one names its
    /// node count, or its commonest reason and how many others there are. A lone suspended job says so
    /// before its nodes.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!(groups[0].place(), "2 nodes");
    /// assert_eq!(groups[1].place(), "Priority");
    /// ```
    pub fn place(&self) -> String {
        match self.key.phase {
            Phase::Running => {
                let nodes: BTreeSet<&NodeName> = self
                    .attempts
                    .iter()
                    .flat_map(|attempt| &attempt.nodes)
                    .collect();
                if let [attempt] = self.attempts.as_slice() {
                    let names = nodes
                        .iter()
                        .map(|node| node.0.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    if attempt.state == JobState::Suspended {
                        format!("suspended · {names}")
                    } else {
                        names
                    }
                } else {
                    format!(
                        "{} node{}",
                        nodes.len(),
                        if nodes.len() == 1 { "" } else { "s" }
                    )
                }
            }
            Phase::Pending => {
                let mut reasons: BTreeMap<&str, u64> = BTreeMap::new();
                for attempt in &self.attempts {
                    *reasons
                        .entry(attempt.reason.as_deref().unwrap_or("–"))
                        .or_default() += u64::from(attempt.tasks());
                }
                let others = reasons.len() - 1;
                let commonest = reasons
                    .into_iter()
                    .max_by_key(|(reason, tasks)| (*tasks, std::cmp::Reverse(*reason)))
                    .map_or("–", |(reason, _)| reason);
                if others == 0 {
                    commonest.to_owned()
                } else {
                    format!("{commonest} +{others}")
                }
            }
        }
    }

    /// How long a lone running attempt has left at `at`, in seconds, or `None` for a group, a pending
    /// or suspended attempt, or one with no time limit.
    ///
    /// # Examples
    ///
    /// ```
    /// # let mut cluster = glurmo_model::jobs::example();
    /// use glurmo_model::{JobId, UnixTime};
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// for running in cluster.running.values_mut() {
    ///     running.attempt.end = Some(UnixTime(1_700_003_600));
    /// }
    /// let search = Search { text: "4250", ..Search::default() };
    /// let groups = job_groups(&cluster, &search, Order::default());
    /// assert_eq!(groups[0].left_s(UnixTime(1_700_000_600)), Some(3_000));
    /// ```
    pub fn left_s(&self, at: UnixTime) -> Option<i64> {
        let [attempt] = self.attempts.as_slice() else {
            return None;
        };
        let end = attempt
            .end
            .filter(|_| self.key.phase == Phase::Running && attempt.state != JobState::Suspended)?;
        Some((end.0 - at.0).max(0))
    }

    /// When the earliest attempt was submitted.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::UnixTime;
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert_eq!(groups[0].submitted(), Some(UnixTime(1_700_000_000)));
    /// ```
    pub fn submitted(&self) -> Option<UnixTime> {
        self.attempts.iter().map(|attempt| attempt.key.submit).min()
    }

    /// Whether `key` is one of the group's attempts.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::{Order, Search, job_groups};
    ///
    /// let groups = job_groups(&cluster, &Search::default(), Order::default());
    /// assert!(groups[0].holds(groups[0].attempts[1].key));
    /// assert!(!groups[1].holds(groups[0].attempts[1].key));
    /// ```
    pub fn holds(&self, key: AttemptKey) -> bool {
        self.attempts.iter().any(|attempt| attempt.key == key)
    }

    /// Sums `per_task` over every task.
    fn sum(&self, per_task: impl Fn(&Attempt) -> u64) -> u64 {
        self.attempts
            .iter()
            .map(|attempt| per_task(attempt) * u64::from(attempt.tasks()))
            .sum()
    }
}

/// The running and pending attempts `search` lets through, grouped, running groups first and each
/// phase in `order`.
///
/// # Examples
///
/// ```
/// # let cluster = glurmo_model::jobs::example();
/// use glurmo_model::jobs::{Order, Phase, Search, job_groups};
///
/// let all = job_groups(&cluster, &Search::default(), Order::default());
/// assert_eq!(all.len(), 2);
/// let found = job_groups(&cluster, &Search { text: "OTTER z2", ..Search::default() }, Order::default());
/// assert_eq!((found.len(), found[0].key.phase), (1, Phase::Running));
/// let pending = Search { phase: Some(Phase::Pending), ..Search::default() };
/// assert_eq!(job_groups(&cluster, &pending, Order::default())[0].tasks(), 4);
/// ```
pub fn job_groups<'a>(cluster: &'a Cluster, search: &Search, order: Order) -> Vec<JobGroup<'a>> {
    let words = Words::of(search.text);
    let running = cluster
        .running
        .values()
        .map(|running| (&running.attempt, Phase::Running));
    let pending = cluster
        .queue
        .values()
        .map(|attempt| (attempt, Phase::Pending));
    let mut groups: BTreeMap<GroupKey, Vec<&Attempt>> = BTreeMap::new();
    for (attempt, phase) in running.chain(pending) {
        let shown = search.phase.is_none_or(|only| only == phase)
            && search.login.is_none_or(|login| *login == attempt.login)
            && (!search.gpus || attempt.gpus > 0)
            && words.found_in_attempt(cluster, attempt);
        if shown {
            groups
                .entry(GroupKey::of(attempt, phase))
                .or_default()
                .push(attempt);
        }
    }
    let mut groups: Vec<JobGroup> = groups
        .into_iter()
        .map(|(key, mut attempts)| {
            attempts.sort_by_key(|attempt| attempt.key);
            JobGroup { key, attempts }
        })
        .collect();
    groups.sort_by(|a, b| {
        let within = match order {
            Order::Cpus => b.cpus().cmp(&a.cpus()),
            Order::Gpus => b.gpus().cmp(&a.gpus()),
            Order::Memory => b.mem_mb().cmp(&a.mem_mb()),
            Order::Time => a.since().cmp(&b.since()),
            Order::User => a.key.login.cmp(&b.key.login),
        };
        a.key
            .phase
            .cmp(&b.key.phase)
            .then(within)
            .then_with(|| a.first().key.cmp(&b.first().key))
    });
    groups
}

/// A search's words, without regard to case. A text matches when it holds every word.
///
/// # Examples
///
/// ```
/// use glurmo_model::jobs::Words;
///
/// let words = Words::of("Otter 5days");
/// assert!(words.found_in(&["otter", "Otter Ott", "5days,long"]));
/// assert!(!words.found_in(&["otter", "4hours"]), "every word must appear");
/// assert!(Words::of("  ").found_in(&[]), "no words match everything");
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Words(Vec<String>);

impl Words {
    /// The words of `text`, split on whitespace.
    pub fn of(text: &str) -> Self {
        Self(text.split_whitespace().map(str::to_lowercase).collect())
    }

    /// Whether every word appears in one of `texts`.
    pub fn found_in(&self, texts: &[&str]) -> bool {
        self.0.is_empty() || {
            let texts: Vec<String> = texts.iter().map(|text| text.to_lowercase()).collect();
            self.0
                .iter()
                .all(|word| texts.iter().any(|text| text.contains(word.as_str())))
        }
    }

    /// Whether every word appears in what the job list shows of `attempt`: its login, full name, ID,
    /// name, partitions, nodes or reason.
    ///
    /// # Examples
    ///
    /// ```
    /// # let cluster = glurmo_model::jobs::example();
    /// use glurmo_model::jobs::Words;
    ///
    /// let pending = cluster.queue.values().next().unwrap();
    /// assert!(Words::of("4242 priority").found_in_attempt(&cluster, pending));
    /// assert!(!Words::of("z1").found_in_attempt(&cluster, pending), "it runs nowhere yet");
    /// ```
    pub fn found_in_attempt(&self, cluster: &Cluster, attempt: &Attempt) -> bool {
        if self.0.is_empty() {
            return true;
        }
        let name = cluster
            .users
            .get(&attempt.login)
            .and_then(|user| user.name.as_deref());
        let id = attempt.printed_id();
        let partitions = partitions(attempt);
        let mut texts = vec![
            attempt.login.0.as_str(),
            name.unwrap_or(""),
            &id,
            &attempt.name,
            &partitions,
            attempt.reason.as_deref().unwrap_or(""),
        ];
        texts.extend(attempt.nodes.iter().map(|node| node.0.as_str()));
        self.found_in(&texts)
    }
}

fn partitions(attempt: &Attempt) -> String {
    attempt
        .partitions
        .iter()
        .map(|partition| partition.0.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// A cluster for the examples: two running `align` jobs of 8 CPUs, one GPU and 16 GB each on `z1` and
/// `z2`, and a pending array of four 1-CPU tasks waiting on priority, all `otter`'s.
#[doc(hidden)]
pub fn example() -> Cluster {
    let align = |job: u32| Attempt {
        key: AttemptKey {
            job: JobId(job),
            submit: UnixTime(1_700_000_000),
        },
        array: None,
        login: Login("otter".into()),
        partitions: vec![PartitionName("5days".into())],
        state: JobState::Running,
        state_raw: "RUNNING".into(),
        cpus: 8,
        mem_mb: Some(16_000),
        gpus: 1,
        time_limit_s: None,
        start: None,
        end: None,
        nodes: Vec::new(),
        reason: None,
        name: "align".into(),
    };
    let mut cluster = Cluster::default();
    for (job, start, node) in [(4250, 1_700_000_100, "z1"), (4251, 1_700_000_200, "z2")] {
        let attempt = Attempt {
            start: Some(UnixTime(start)),
            nodes: vec![NodeName(node.into())],
            ..align(job)
        };
        let running = Running {
            attempt,
            placement: None,
        };
        cluster.running.insert(running.attempt.key, running);
    }
    let pending = Attempt {
        array: Some(ArrayTasks {
            job: JobId(4242),
            spec: "0-3".into(),
            count: 4,
        }),
        state: JobState::Pending,
        state_raw: "PENDING".into(),
        cpus: 1,
        mem_mb: None,
        gpus: 0,
        reason: Some("Priority".into()),
        ..align(4242)
    };
    cluster.queue.insert(pending.key, pending);
    cluster
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suspended_job_reads_as_suspended_with_no_countdown() {
        let mut cluster = example();
        for running in cluster.running.values_mut() {
            running.attempt.end = Some(UnixTime(1_700_003_600));
            running.attempt.state = JobState::Suspended;
        }
        let search = Search {
            text: "4250",
            ..Search::default()
        };
        let groups = job_groups(&cluster, &search, Order::default());
        assert_eq!(groups[0].left_s(UnixTime(1_700_000_600)), None);
        assert_eq!(groups[0].place(), "suspended · z1");
    }

    #[test]
    fn running_groups_come_first_whatever_the_order() {
        let cluster = example();
        for order in [
            Order::Cpus,
            Order::Gpus,
            Order::Memory,
            Order::Time,
            Order::User,
        ] {
            let groups = job_groups(&cluster, &Search::default(), order);
            let phases: Vec<Phase> = groups.iter().map(|group| group.key.phase).collect();
            assert_eq!(phases, [Phase::Running, Phase::Pending], "{order:?}");
        }
    }

    #[test]
    fn filters_narrow_by_gpus_and_owner() {
        let cluster = example();
        let gpus = Search {
            gpus: true,
            ..Search::default()
        };
        assert_eq!(job_groups(&cluster, &gpus, Order::Cpus).len(), 1);
        let wren = Login("wren".into());
        let theirs = Search {
            login: Some(&wren),
            ..Search::default()
        };
        assert_eq!(job_groups(&cluster, &theirs, Order::Cpus).len(), 0);
        let words = Search {
            text: "align priority",
            ..Search::default()
        };
        let found = job_groups(&cluster, &words, Order::Cpus);
        assert_eq!(found[0].label(), "4242_0-3", "every word must match");
    }
}
