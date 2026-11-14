//! Exact placement, partitions and site facts from scontrol.

use std::collections::HashMap;
use std::ops::Range;

use anyhow::{Context, Result};
use serde::Serialize;

use super::fields::{
    gpu_indices, gres, integer, is_placeholder, job_state, megabytes, seconds, time,
};
use super::hostlist::{expand_hosts, expand_ids};
use super::{RateLimit, Site, Version};
use crate::model::{
    Attempt, JobId, JobState, Node, NodeCells, NodeName, Partition, PartitionName, Placement,
    UnixTime,
};

/// One line of `scontrol -o` output, split into `Key=value` pairs.
///
/// Values can hold spaces, so a token that does not look like `Key=` continues the value before it.
/// `JobName` is free text and runs up to ` UserId=`.
pub struct Record<'a> {
    line: &'a str,
    pairs: Vec<(&'a str, Range<usize>)>,
}

impl<'a> Record<'a> {
    /// Splits a line. Fields are separated by runs of whitespace, because 25.11 changed the spacing.
    pub fn parse(line: &'a str) -> Self {
        let mut pairs: Vec<(&str, Range<usize>)> = Vec::new();
        for (start, token) in tokens(line) {
            let in_job_name = pairs.last().is_some_and(|(key, _)| *key == "JobName")
                && !token.starts_with("UserId=");
            match token.split_once('=') {
                Some((key, _)) if !in_job_name && is_key(key) => {
                    pairs.push((key, start + key.len() + 1..start + token.len()));
                }
                _ => {
                    if let Some((_, value)) = pairs.last_mut() {
                        value.end = start + token.len();
                    }
                }
            }
        }
        Self { line, pairs }
    }

    /// The first value under `key`.
    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.pairs
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| &self.line[value.clone()])
    }

    /// The first value under `key`.
    ///
    /// # Errors
    ///
    /// Fails when the line has no such key.
    pub fn require(&self, key: &str) -> Result<&'a str> {
        self.get(key).with_context(|| format!("no {key}="))
    }

    /// Every pair in line order.
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, &'a str)> + '_ {
        self.pairs
            .iter()
            .map(|(key, value)| (*key, &self.line[value.clone()]))
    }
}

fn tokens(line: &str) -> Vec<(usize, &str)> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices().chain([(line.len(), ' ')]) {
        match (c.is_whitespace(), start) {
            (true, Some(from)) => {
                tokens.push((from, &line[from..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    tokens
}

fn is_key(key: &str) -> bool {
    key.starts_with(|c: char| c.is_ascii_alphabetic())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '/'))
}

/// The command that prints every job with its placement, for a cold start.
pub fn jobs_argv() -> Vec<String> {
    ["scontrol", "-d", "-o", "show", "job"]
        .map(String::from)
        .to_vec()
}

/// The command that looks up one job's placement.
///
/// An array task is looked up as `ID_TASK`, because its ID may be the array's base ID, which returns every task.
pub fn lookup_argv(attempt: &Attempt) -> Vec<String> {
    let id = match &attempt.array {
        Some(array) => format!("{}_{}", array.job.0, array.spec),
        None => attempt.key.job.0.to_string(),
    };
    let mut argv = jobs_argv();
    argv.push(id);
    argv
}

/// What scontrol prints on stderr for a job the controller no longer knows.
pub const GONE: &str = "Invalid job id specified";

/// A job's placement as scontrol reports it.
#[derive(Debug, PartialEq, Serialize)]
pub struct Placed {
    /// The job ID.
    pub job: JobId,
    /// When it started, to tell a lookup that raced a requeue.
    pub start: Option<UnixTime>,
    /// Its state when scontrol ran.
    pub state: JobState,
    /// Its cores and GPUs. Empty for a job that holds none.
    pub placement: Placement,
}

/// What turns a node's abstract CPU IDs into core indices.
pub struct CpuMap {
    cpus_per_core: HashMap<NodeName, u32>,
    truncates: bool,
}

/// Before 25.11, scontrol formats `CPU_IDs` into a 128-byte buffer and then drops the last character.
/// A list cut short is therefore 126 characters long, and its last item may be a fragment.
const CUT_CPU_IDS: usize = 126;

impl CpuMap {
    /// Reads each node's CPUs per core, and whether this Slurm version cuts long `CPU_IDs` lists short.
    pub fn new(nodes: &[Node], version: Version) -> Self {
        Self {
            cpus_per_core: nodes
                .iter()
                .map(|node| (node.name.clone(), node.cpus_per_core()))
                .collect(),
            truncates: version < Version(25, 11, 0),
        }
    }
}

/// Parses the output of [`jobs_argv`] or [`lookup_argv`], one job per line.
///
/// # Errors
///
/// Fails on a line without the job fields, a bad value, or a node that sinfo did not list.
pub fn parse_jobs(stdout: &str, map: &CpuMap) -> Result<Vec<Placed>> {
    stdout
        .lines()
        .map(|line| placed(line, map).with_context(|| format!("scontrol line {line:?}")))
        .collect()
}

struct Group<'a> {
    hosts: Vec<NodeName>,
    cpu_ids: Vec<u32>,
    cut: bool,
    mem: Option<&'a str>,
    gres: Option<&'a str>,
}

fn placed(line: &str, map: &CpuMap) -> Result<Placed> {
    let record = Record::parse(line);
    let pairs: Vec<(&str, &str)> = record
        .iter()
        .take_while(|(key, _)| *key != "MinCPUsNode")
        .collect();
    let mut groups = Vec::new();
    for (i, &(key, hosts)) in pairs.iter().enumerate() {
        if key != "Nodes" {
            continue;
        }
        let Some(&("CPU_IDs", cpu_ids)) = pairs.get(i + 1) else {
            continue;
        };
        let follows = |offset: usize, name: &str| {
            pairs
                .get(i + offset)
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| *value)
        };
        let cut = map.truncates && cpu_ids.len() >= CUT_CPU_IDS;
        let whole = if cut {
            cpu_ids.rsplit_once(',').map_or("", |(whole, _)| whole)
        } else {
            cpu_ids
        };
        groups.push(Group {
            hosts: expand_hosts(hosts)?,
            cpu_ids: expand_ids(whole)?,
            cut,
            mem: follows(2, "Mem"),
            gres: follows(3, "GRES"),
        });
    }
    let num_cpus: u32 = integer(record.require("NumCPUs")?)?;
    Ok(Placed {
        job: JobId(integer(record.require("JobId")?)?),
        start: time(record.require("StartTime")?)?,
        state: job_state(record.require("JobState")?)?,
        placement: placement(&groups, num_cpus, map)?,
    })
}

/// Turns CPU IDs into cores. When a group's IDs were cut short, its nodes share the job's remaining CPUs evenly, and the cores
/// not listed count as unplaced.
fn placement(groups: &[Group], num_cpus: u32, map: &CpuMap) -> Result<Placement> {
    let host_count = |group: &Group| u32::try_from(group.hosts.len()).unwrap_or(u32::MAX);
    let listed: u32 = groups
        .iter()
        .filter(|group| !group.cut)
        .map(|group| u32::try_from(group.cpu_ids.len()).unwrap_or(u32::MAX) * host_count(group))
        .sum();
    let cut_hosts: u32 = groups
        .iter()
        .filter(|group| group.cut)
        .map(host_count)
        .sum();
    let cpus_per_cut_host = num_cpus.saturating_sub(listed).div_ceil(cut_hosts.max(1));
    let mut cells = Vec::new();
    for group in groups {
        let gpus = gpu_indices(&gres(group.gres.unwrap_or_default())?)?;
        let mem_mb = group.mem.map(megabytes).transpose()?.flatten();
        for host in &group.hosts {
            let cpus_per_core = *map
                .cpus_per_core
                .get(host)
                .with_context(|| format!("node {} is missing from sinfo", host.0))?;
            let mut cores: Vec<u32> = group
                .cpu_ids
                .iter()
                .map(|cpu| cpu / cpus_per_core)
                .collect();
            cores.dedup();
            let unplaced = if group.cut {
                let expected = cpus_per_cut_host.div_ceil(cpus_per_core);
                expected.saturating_sub(u32::try_from(cores.len()).unwrap_or(u32::MAX))
            } else {
                0
            };
            cells.push(NodeCells {
                node: host.clone(),
                cores,
                unplaced,
                gpus: gpus.clone(),
                mem_mb,
            });
        }
    }
    Ok(Placement(cells))
}

/// The command that lists partitions, hidden ones included.
pub fn partitions_argv() -> Vec<String> {
    ["scontrol", "-a", "-o", "show", "partition"]
        .map(String::from)
        .to_vec()
}

/// Parses the output of [`partitions_argv`].
///
/// # Errors
///
/// Fails on a line without the partition fields or with a bad value.
pub fn parse_partitions(stdout: &str) -> Result<Vec<Partition>> {
    stdout
        .lines()
        .map(|line| partition(line).with_context(|| format!("scontrol line {line:?}")))
        .collect()
}

fn partition(line: &str) -> Result<Partition> {
    let record = Record::parse(line);
    Ok(Partition {
        name: PartitionName(record.require("PartitionName")?.to_owned()),
        nodes: expand_hosts(record.require("Nodes")?)?,
        max_time_s: seconds(record.require("MaxTime")?)?,
        default: record.require("Default")? == "YES",
        hidden: record.require("Hidden")? == "YES",
        state_raw: record.require("State")?.to_owned(),
    })
}

/// The command that prints the controller's configuration.
pub fn config_argv() -> Vec<String> {
    ["scontrol", "show", "config"].map(String::from).to_vec()
}

/// Parses the output of [`config_argv`].
///
/// # Errors
///
/// Fails when a needed setting is missing or does not parse.
pub fn parse_config(stdout: &str) -> Result<Site> {
    let settings: HashMap<&str, &str> = stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect();
    let setting = |key: &str| {
        settings
            .get(key)
            .copied()
            .with_context(|| format!("scontrol show config has no {key}"))
    };
    let min_job_age = setting("MinJobAge")?;
    let private_data = setting("PrivateData")?;
    Ok(Site {
        cluster: setting("ClusterName")?.to_owned(),
        version: setting("SLURM_VERSION")?.parse()?,
        min_job_age_s: integer(min_job_age.trim_end_matches(" sec"))
            .with_context(|| format!("bad MinJobAge {min_job_age:?}"))?,
        private_data: if is_placeholder(private_data) {
            Vec::new()
        } else {
            private_data.split(',').map(str::to_owned).collect()
        },
        rate_limit: rate_limit(setting("SlurmctldParameters")?)?,
    })
}

/// Reads the `rl_*` settings, which matter only once `rl_enable` is set. Missing ones take Slurm's defaults.
fn rate_limit(parameters: &str) -> Result<Option<RateLimit>> {
    let parameters: Vec<&str> = parameters.split(',').collect();
    if !parameters.contains(&"rl_enable") {
        return Ok(None);
    }
    let mut limit = RateLimit {
        bucket_size: 30,
        refill_rate: 2,
        refill_period_s: 1,
    };
    for parameter in parameters {
        match parameter.split_once('=') {
            Some(("rl_bucket_size", value)) => limit.bucket_size = integer(value)?,
            Some(("rl_refill_rate", value)) => limit.refill_rate = integer(value)?,
            Some(("rl_refill_period", value)) => limit.refill_period_s = integer(value)?,
            _ => {}
        }
    }
    Ok(Some(limit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EndState;

    const RUNNING: &str = "JobId=3249466 JobName=bash UserId=otter(20001) GroupId=otter(20001) MCS_label=N/A Priority=1 Nice=0 Account=(null) QOS=normal JobState=RUNNING Reason=None Dependency=(null) Requeue=1 Restarts=0 BatchFlag=0 Reboot=0 ExitCode=0:0 DerivedExitCode=0:0 RunTime=46-09:38:24 TimeLimit=60-00:00:00 TimeMin=N/A SubmitTime=1790684947 EligibleTime=1790684947 AccrueTime=Unknown StartTime=1790684947 EndTime=1795868947 Deadline=N/A SuspendTime=None SecsPreSuspend=0 LastSchedEval=1790684947 Scheduler=Main Partition=long AllocNode:Sid=z012:657564 ReqNodeList=(null) ExcNodeList=z1059 NodeList=z1054 BatchHost=z1054 NumNodes=1 NumCPUs=72 NumTasks=1 CPUs/Task=1 ReqB:S:C:T=0:0:*:* ReqTRES=cpu=72,mem=1000000M,node=1,billing=72 AllocTRES=cpu=72,mem=1000000M,node=1,billing=72 Socks/Node=* NtasksPerN:B:S:C=0:0:*:* CoreSpec=* JOB_GRES=(null)   Nodes=z1054 CPU_IDs=0-71 Mem=1000000 GRES= MinCPUsNode=1 MinMemoryNode=0 MinTmpDiskNode=0 Features=(null) DelayBoot=00:00:00 OverSubscribe=NO Contiguous=0 Licenses=(null) Network=(null) Command=bash WorkDir=/home/otter Power= ";

    const GPU: &str = "JobId=3923677 JobName=job3 UserId=lynx(20003) GroupId=lynx(20003) MCS_label=N/A Priority=1 Nice=0 Account=(null) QOS=normal JobState=RUNNING Reason=None Dependency=(null) Requeue=1 Restarts=0 BatchFlag=1 Reboot=0 ExitCode=0:0 DerivedExitCode=0:0 RunTime=1-23:00:00 TimeLimit=14-00:00:00 TimeMin=N/A SubmitTime=1794524389 EligibleTime=1794524389 AccrueTime=1794524389 StartTime=1794524389 EndTime=1795733989 Deadline=N/A SuspendTime=None SecsPreSuspend=0 LastSchedEval=1794524389 Scheduler=Main Partition=gpuh200 AllocNode:Sid=z012:1 ReqNodeList=(null) ExcNodeList=(null) NodeList=z028 BatchHost=z028 NumNodes=1 NumCPUs=32 NumTasks=1 CPUs/Task=32 ReqB:S:C:T=0:0:*:* ReqTRES=cpu=32,mem=256G,node=1,billing=32,gres/gpu=4 AllocTRES=cpu=32,mem=256G,node=1,billing=32,gres/gpu=4 Socks/Node=* NtasksPerN:B:S:C=0:0:*:* CoreSpec=* JOB_GRES=gpu:4   Nodes=z028 CPU_IDs=0-31 Mem=262144 GRES=gpu:4(IDX:0-3) MinCPUsNode=32 MinMemoryNode=256G MinTmpDiskNode=0 Features=(null) DelayBoot=00:00:00 OverSubscribe=OK Contiguous=0 Licenses=(null) Network=(null) Command=/redacted WorkDir=/redacted Power= TresPerNode=gres/gpu:4";

    const PENDING_RANGE: &str = "JobId=3441951 ArrayJobId=3441951 ArrayTaskId=0-3 JobName=job4 UserId=wren(20004) GroupId=wren(20004) MCS_label=N/A Priority=1 Nice=0 Account=(null) QOS=normal JobState=PENDING Reason=Resources Dependency=(null) Requeue=1 Restarts=0 BatchFlag=1 Reboot=0 ExitCode=0:0 DerivedExitCode=0:0 RunTime=00:00:00 TimeLimit=00:05:00 TimeMin=N/A SubmitTime=1792077150 EligibleTime=1792077150 AccrueTime=1792077150 StartTime=1795868947 EndTime=1795869247 Deadline=N/A SuspendTime=None SecsPreSuspend=0 LastSchedEval=1794693995 Scheduler=Main Partition=long AllocNode:Sid=z010:2321916 ReqNodeList=z1054 ExcNodeList=(null) NodeList= NumNodes=1 NumCPUs=1 NumTasks=1 CPUs/Task=1 ReqB:S:C:T=0:0:*:* ReqTRES=cpu=2,mem=1G,node=1,billing=2 AllocTRES=(null) Socks/Node=* NtasksPerN:B:S:C=0:0:*:* CoreSpec=* MinCPUsNode=1 MinMemoryNode=1G MinTmpDiskNode=0 Features=(null) DelayBoot=00:00:00 OverSubscribe=OK Contiguous=0 Licenses=(null) Network=(null) Command=/redacted WorkDir=/redacted StdErr=/redacted StdIn=/dev/null StdOut=/redacted Power= TresPerTask=cpu:1 ";

    fn map(nodes: &[(&str, u32)], version: Version) -> CpuMap {
        CpuMap {
            cpus_per_core: nodes
                .iter()
                .map(|(name, cpus)| (NodeName((*name).to_owned()), *cpus))
                .collect(),
            truncates: version < Version(25, 11, 0),
        }
    }

    fn zlurm() -> CpuMap {
        map(
            &[
                ("z1054", 2),
                ("z028", 2),
                ("z1060", 2),
                ("z1061", 2),
                ("z1062", 2),
            ],
            Version(23, 11, 4),
        )
    }

    fn one(line: &str) -> Placed {
        parse_jobs(line, &zlurm()).unwrap().remove(0)
    }

    #[test]
    fn values_with_spaces_continue_until_the_next_key() {
        let record =
            Record::parse("Reason=ReqNodeNotAvail, UnavailableNodes:z1018 Dependency=(null)");
        assert_eq!(
            record.get("Reason"),
            Some("ReqNodeNotAvail, UnavailableNodes:z1018")
        );
        assert_eq!(record.get("Dependency"), Some("(null)"));
    }

    #[test]
    fn job_name_cannot_inject_keys() {
        let record = Record::parse("JobId=1 JobName=x  NumCPUs=99 Nodes=z1 UserId=a(1) NumCPUs=2");
        assert_eq!(record.get("JobName"), Some("x  NumCPUs=99 Nodes=z1"));
        assert_eq!(record.get("NumCPUs"), Some("2"));
    }

    #[test]
    fn cpu_ids_become_cores() {
        let job = one(RUNNING);
        assert_eq!((job.job, job.state), (JobId(3_249_466), JobState::Running));
        assert_eq!(job.start, Some(UnixTime(1_790_684_947)));
        let [cells] = &job.placement.0[..] else {
            panic!("one node")
        };
        assert_eq!(cells.node, NodeName("z1054".into()));
        assert_eq!(cells.cores, (0..36).collect::<Vec<_>>());
        assert_eq!((cells.unplaced, cells.mem_mb), (0, Some(1_000_000)));
        assert_eq!(cells.gpus, Vec::<u32>::new());
        assert!(job.placement.is_exact());
    }

    #[test]
    fn gpu_indices_come_from_gres_idx() {
        let job = one(GPU);
        let cells = &job.placement.0[0];
        assert_eq!(cells.gpus, [0, 1, 2, 3]);
        assert_eq!(cells.cores, (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn pending_jobs_hold_nothing() {
        let job = one(PENDING_RANGE);
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.placement, Placement(Vec::new()));
    }

    #[test]
    fn multi_node_groups_share_or_split_cpu_ids() {
        let line = "JobId=8 JobName=mpi UserId=yak(1) JobState=RUNNING StartTime=1794244136 NumCPUs=260 JOB_GRES=gpu:a100:3 Nodes=z[1060-1061] CPU_IDs=0-103 Mem=500000 GRES=gpu:a100:2(IDX:0,2) Nodes=z1062 CPU_IDs=0-51 Mem=250000 GRES=gpu:a100:1(IDX:1) MinCPUsNode=1";
        let cells = one(line).placement.0;
        let nodes: Vec<&str> = cells.iter().map(|cells| cells.node.0.as_str()).collect();
        assert_eq!(nodes, ["z1060", "z1061", "z1062"]);
        assert_eq!(cells[1].cores.len(), 52);
        assert_eq!(cells[1].gpus, [0, 2]);
        assert_eq!(
            (cells[2].cores.len(), cells[2].gpus.as_slice()),
            (26, &[1][..])
        );
        assert_eq!(cells[2].mem_mb, Some(250_000));
    }

    #[test]
    fn spacing_from_25_11_parses_the_same() {
        let line = "JobId=3249466\tJobName=bash UserId=otter(1) JobState=RUNNING StartTime=1790684947 NumCPUs=72 JOB_GRES=(null) Nodes=z1054    CPU_IDs=0-71  Mem=1000000 GRES= MinCPUsNode=1";
        assert_eq!(one(line).placement, one(RUNNING).placement);
    }

    /// `CPU_IDs` formatted the way Slurm 23.11 cuts them: 127 characters into the buffer, then the last one zapped.
    fn cut_cpu_ids(cpu_ids: &str) -> &str {
        &cpu_ids[..CUT_CPU_IDS]
    }

    #[test]
    fn truncated_cpu_ids_pack_the_rest() {
        let full: Vec<String> = (0..64)
            .map(|core| format!("{}-{}", 4 * core, 4 * core + 1))
            .collect();
        let full = full.join(",");
        let line = format!(
            "JobId=5 JobName=frag UserId=a(1) JobState=RUNNING StartTime=1 NumCPUs=128 JOB_GRES=(null) Nodes=z028 CPU_IDs={} Mem=1 GRES= MinCPUsNode=1",
            cut_cpu_ids(&full)
        );
        let placement = one(&line).placement;
        let cells = &placement.0[0];
        assert!(!placement.is_exact());
        assert_eq!(
            u32::try_from(cells.cores.len()).unwrap() + cells.unplaced,
            64
        );
        let true_cores: Vec<u32> = (0..64).map(|core| 2 * core).collect();
        assert!(cells.cores.iter().all(|core| true_cores.contains(core)));
    }

    #[test]
    fn long_cpu_ids_from_25_11_are_whole() {
        let full: Vec<String> = (0..64)
            .map(|core| format!("{}-{}", 4 * core, 4 * core + 1))
            .collect();
        let line = format!(
            "JobId=5 JobName=frag UserId=a(1) JobState=RUNNING StartTime=1 NumCPUs=128 Nodes=z028 CPU_IDs={} Mem=1 GRES= MinCPUsNode=1",
            full.join(",")
        );
        let placement = parse_jobs(&line, &map(&[("z028", 2)], Version(25, 11, 0)))
            .unwrap()
            .remove(0)
            .placement;
        assert!(placement.is_exact());
        assert_eq!(placement.0[0].cores.len(), 64);
    }

    #[test]
    fn nodes_missing_from_sinfo_fail() {
        let line = RUNNING.replace("z1054", "z9999");
        assert!(parse_jobs(&line, &zlurm()).is_err());
    }

    #[test]
    fn ended_records_keep_their_placement() {
        let line = RUNNING
            .replace("JobState=RUNNING", "JobState=COMPLETED")
            .replace(
                "JobId=3249466",
                "JobId=3848210 ArrayJobId=3848210 ArrayTaskId=174",
            );
        let job = one(&line);
        assert_eq!(job.state, JobState::Ended(EndState::Completed));
        assert_eq!(job.placement.0[0].cores.len(), 36);
    }

    #[test]
    fn partitions() {
        let line = "PartitionName=30mins AllowGroups=ALL AllowAccounts=ALL AllowQos=ALL AllocNodes=ALL Default=YES QoS=N/A DefaultTime=00:10:00 DisableRootJobs=NO ExclusiveUser=NO GraceTime=0 Hidden=NO MaxNodes=UNLIMITED MaxTime=00:30:00 MinNodes=0 LLN=YES MaxCPUsPerNode=UNLIMITED MaxCPUsPerSocket=UNLIMITED Nodes=z[1014,1016-1017] PriorityJobFactor=5 PriorityTier=1 RootOnly=NO ReqResv=NO OverSubscribe=NO OverTimeLimit=NONE PreemptMode=OFF State=UP TotalCPUs=264 TotalNodes=3 SelectTypeParameters=NONE JobDefaults=(null) DefMemPerNode=16384 MaxMemPerNode=UNLIMITED TRES=cpu=264,mem=1500000M,node=3,billing=264";
        let [partition] = &parse_partitions(line).unwrap()[..] else {
            panic!("one")
        };
        assert_eq!(partition.name, PartitionName("30mins".into()));
        assert_eq!(partition.nodes.len(), 3);
        assert_eq!(partition.max_time_s, Some(1800));
        assert!(partition.default && !partition.hidden);
        assert_eq!(partition.state_raw, "UP");
    }

    const CONFIG: &str = "\
Configuration data as of 1794694066
AccountingStorageTRES   = cpu,mem,energy,node,billing,fs/disk,vmem,pages,gres/gpu,gres/gpumem,gres/gpuutil
ClusterName             = zlurm
MinJobAge               = 300 sec
PrivateData             = none
SlurmctldParameters     = (null)
SLURM_VERSION           = 23.11.4

Slurmctld(primary) at slurmd-211 is UP
";

    #[test]
    fn site_facts() {
        let site = parse_config(CONFIG).unwrap();
        assert_eq!(site.cluster, "zlurm");
        assert_eq!(site.version, Version(23, 11, 4));
        assert_eq!(site.min_job_age_s, 300);
        assert_eq!(site.private_data, Vec::<String>::new());
        assert_eq!(site.rate_limit, None);
    }

    #[test]
    fn private_data_and_rate_limits() {
        let config = CONFIG
            .replace("= none", "= jobs,usage")
            .replace("= (null)", "= enable_configless,rl_enable,rl_refill_rate=5");
        let site = parse_config(&config).unwrap();
        assert_eq!(site.private_data, ["jobs", "usage"]);
        assert_eq!(
            site.rate_limit,
            Some(RateLimit {
                bucket_size: 30,
                refill_rate: 5,
                refill_period_s: 1
            })
        );
    }
}
