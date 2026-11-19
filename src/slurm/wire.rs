//! The protocol backend: asks slurmctld and slurmdbd through the RPC client, and maps its records to
//! the model.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

use super::Lookup;
use super::fields::{gpu_count, gpu_indices, gpu_model, gres, job_state, node_state, tres};
use super::hostlist::expand_hosts;
use super::recording::{Answer, Call};
use super::rpc::{Client, INFINITE, Issuer, NO_VAL, accounting, config, jobs, nodes, stats};
use super::sacct::Outcome;
use super::scontrol::Placed;
use crate::logging::Streak;
use glurmo_model::{
    ArrayTasks, Attempt, AttemptKey, JobId, Login, Node, NodeCells, NodeName, Partition,
    PartitionName, Placement, UnixTime,
};

/// The site facts in the configuration.
fn site(config: config::Config) -> Result<super::Site> {
    Ok(super::Site {
        cluster: config.cluster,
        version: config.version.parse()?,
        min_job_age_s: u64::from(config.min_job_age_s),
        private_data: config.private_data,
        rate_limit: super::rate_limit(config.slurmctld_parameters.as_deref().unwrap_or_default())?,
    })
}

/// The node as the model holds it.
fn node(node: nodes::Node) -> Result<Node> {
    let (state, flags) = node_state(&node.state);
    let gres_total = gres(&node.gres).with_context(|| format!("node {}", node.name))?;
    let gres_used = gres(&node.gres_used).with_context(|| format!("node {}", node.name))?;
    Ok(Node {
        partitions: node.partitions.into_iter().map(PartitionName).collect(),
        state,
        flags,
        state_raw: node.state,
        reason: node.reason,
        cpus: node.cpus,
        cpus_alloc: node.cpus_alloc,
        sockets: node.sockets,
        cores: node.cores,
        load: node.load,
        mem_mb: node.mem_mb,
        mem_alloc_mb: node.mem_alloc_mb,
        mem_free_mb: node.mem_free_mb,
        gpus: gpu_count(&gres_total),
        gpus_used: gpu_count(&gres_used),
        gpu_model: gpu_model(&gres_total),
        features: node.features,
        name: NodeName(node.name),
    })
}

/// The partition as the model holds it.
fn partition(partition: nodes::Partition) -> Result<Partition> {
    Ok(Partition {
        nodes: expand_hosts(&partition.nodes)
            .with_context(|| format!("partition {}", partition.name))?,
        name: PartitionName(partition.name),
        max_time_s: partition.max_time_s,
        default: partition.default,
        hidden: partition.hidden,
        state_raw: partition.state.to_owned(),
    })
}

/// The counters as the model holds them.
fn stats(stats: stats::Stats) -> glurmo_model::Stats {
    glurmo_model::Stats {
        server_threads: stats.server_threads,
        agent_queue: stats.agent_queue,
        dbd_queue: stats.dbd_queue,
        jobs_submitted: stats.jobs_submitted,
        jobs_started: stats.jobs_started,
        jobs_completed: stats.jobs_completed,
        main_cycle_mean_us: stats.main_cycle_mean_us,
        main_queue_last: stats.main_queue_last,
        backfill_cycle_mean_us: stats.backfill_cycle_mean_us,
        backfilled_jobs: stats.backfilled_jobs,
        rpcs: stats
            .rpcs
            .into_iter()
            .map(|rpc| glurmo_model::RpcCount {
                kind: rpc.kind,
                count: rpc.count,
                total_us: rpc.total_us,
            })
            .collect(),
    }
}

/// The job as the model lists it. `logins` names its owner, as squeue does from the local account
/// database, because the controller's own name for a UID can differ. `now` decides whether an
/// unlimited job's end is only a guess.
fn attempt(job: &jobs::Job, now: UnixTime, logins: &HashMap<u32, Login>) -> Result<Attempt> {
    let tres = tres(&job.tres)?;
    let unlimited_end = job.time_limit == INFINITE && job.end > now.0;
    Ok(Attempt {
        key: AttemptKey {
            job: JobId(job.id),
            submit: epoch(job.submit).context("no submit time")?,
        },
        array: job.array.clone().map(tasks),
        login: logins.get(&job.user_id).cloned().unwrap_or_else(|| {
            Login(
                job.user_name
                    .clone()
                    .unwrap_or_else(|| job.user_id.to_string()),
            )
        }),
        partitions: job
            .partition
            .split(',')
            .map(|name| PartitionName(name.to_owned()))
            .collect(),
        state: job_state(job.state)?,
        state_raw: job.state.to_owned(),
        cpus: job.cpus,
        mem_mb: tres.mem_mb,
        gpus: tres.gpus,
        time_limit_s: (job.time_limit != INFINITE && job.time_limit != NO_VAL)
            .then(|| u64::from(job.time_limit) * 60),
        start: epoch(job.start),
        end: if unlimited_end { None } else { epoch(job.end) },
        nodes: expand_hosts(&job.nodes)?,
        reason: (job.reason != "None" && !job.reason.is_empty()).then(|| job.reason.clone()),
        name: job.name.clone(),
    })
}

/// The array tasks as the model holds them.
fn tasks(tasks: jobs::ArrayTasks) -> ArrayTasks {
    ArrayTasks {
        job: JobId(tasks.job),
        spec: tasks.spec,
        count: tasks.count,
    }
}

/// The job's cores, GPUs and memory on each node, as the model holds them.
fn placed(job: &jobs::Placed) -> Result<Placed> {
    Ok(Placed {
        job: JobId(job.id),
        start: epoch(job.start),
        state: job_state(job.state)?,
        placement: Placement(
            expand_hosts(&job.hosts)?
                .into_iter()
                .zip(&job.cells)
                .map(|(node, cells)| {
                    Ok(NodeCells {
                        node,
                        cores: cells.cores.clone(),
                        unplaced: 0,
                        gpus: gpu_indices(&gres(&cells.gres)?)?,
                        mem_mb: Some(cells.memory_mb),
                    })
                })
                .collect::<Result<_>>()?,
        ),
    })
}

/// A time the controller holds, where zero means none.
fn epoch(time: i64) -> Option<UnixTime> {
    (time != 0).then_some(UnixTime(time))
}

/// What `sacct -X -D -o JobIDRaw,Submit,End,State` reports for a record.
fn outcome(record: &accounting::Record) -> Result<Outcome> {
    Ok(Outcome {
        key: accounted_key(record)?,
        end: accounted_time(record.end),
        state: job_state(record.state)?,
    })
}

/// The allocation as sacct's history columns describe it. A pending job has no allocation yet, so
/// its CPUs, memory and GPUs come from the request. Accounting keeps no reasons, and glurmo keeps
/// no names from it. `logins` names an owner that accounting does not.
fn accounted(
    record: &accounting::Record,
    tres: accounting::Tres,
    logins: &HashMap<u32, Login>,
) -> Result<Attempt> {
    let allocated = tres.counts(record.tres_alloc.as_deref())?;
    let (cpus, counts) = match allocated.cpus {
        Some(cpus) if cpus > 0 => (cpus, allocated),
        _ => (
            u64::from(record.req_cpus),
            tres.counts(record.tres_req.as_deref())?,
        ),
    };
    let array = record.array_tasks()?;
    Ok(Attempt {
        key: accounted_key(record)?,
        array: array.map(tasks),
        login: match &record.user {
            Some(user) if !user.is_empty() => Login(user.clone()),
            _ => logins
                .get(&record.user_id)
                .cloned()
                .unwrap_or_else(|| Login(record.user_id.to_string())),
        },
        partitions: record
            .partition
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(|name| PartitionName(name.to_owned()))
            .collect(),
        state: job_state(record.state)?,
        state_raw: record.state.to_owned(),
        cpus: u32::try_from(cpus)?,
        mem_mb: counts.mem_mb,
        gpus: u32::try_from(counts.gpus)?,
        time_limit_s: (record.time_limit != INFINITE && record.time_limit != NO_VAL)
            .then(|| u64::from(record.time_limit) * 60),
        start: accounted_time(record.start),
        end: accounted_time(record.end),
        nodes: expand_hosts(record.nodes.as_deref().unwrap_or_default())?,
        reason: None,
        name: String::new(),
    })
}

fn accounted_key(record: &accounting::Record) -> Result<AttemptKey> {
    Ok(AttemptKey {
        job: JobId(record.id),
        submit: accounted_time(record.submit).context("no submit time")?,
    })
}

/// A time accounting holds. Zero means none, and so does `NO_VAL`, which slurmdbd stores as the start
/// of a job cancelled while pending.
fn accounted_time(time: i64) -> Option<UnixTime> {
    (time != 0 && time != i64::from(NO_VAL)).then_some(UnixTime(time))
}

/// The accounting records that convert. Those that do not are skipped, so they cannot block the rest,
/// and logged in one line.
fn readable<T>(converted: impl Iterator<Item = Result<T>>) -> Vec<T> {
    let mut skipped = 0;
    let mut first = String::new();
    let records = converted
        .filter_map(|record| {
            record
                .inspect_err(|error| {
                    if skipped == 0 {
                        first = format!("{error:#}");
                    }
                    skipped += 1;
                })
                .ok()
        })
        .collect();
    if skipped > 0 {
        tracing::warn!(
            "skipped {skipped} accounting records that do not convert, the first: {first}"
        );
    }
    records
}

/// Answers calls by speaking Slurm's protocol to slurmctld and slurmdbd.
pub struct Wire {
    slurm: Client,
    logins: HashMap<u32, Login>,
    unknown: HashSet<u32>,
    /// The run of failed login lookups, if any.
    looking_up_logins: Streak,
    /// The placements from the last job listing, for a placement call that comes right after it.
    next_placements: Option<Result<Vec<Placed>>>,
}

impl Wire {
    /// A backend for the slurmctld at `addresses`, primary first, signing with munge or sack as this
    /// host offers.
    ///
    /// # Errors
    ///
    /// Fails when this host runs neither munged nor a sack socket.
    pub fn new(addresses: Vec<String>) -> Result<Self> {
        Ok(Self::over(Client::new(addresses, Issuer::find()?)))
    }

    fn over(slurm: Client) -> Self {
        Self {
            slurm,
            logins: HashMap::new(),
            unknown: HashSet::new(),
            looking_up_logins: Streak::default(),
            next_placements: None,
        }
    }

    /// Answers one call. The config call must come first, because it says who may decode our
    /// credentials and where slurmdbd listens.
    ///
    /// # Errors
    ///
    /// Fails when slurmctld or slurmdbd does not answer, refuses, or sends a reply that does not decode.
    pub async fn answer(&mut self, call: &Call) -> Result<Answer> {
        let listed = self.next_placements.take();
        Ok(match call {
            Call::Config => Answer::Config(site(self.slurm.connect().await?)?),
            Call::Jobs => {
                let (jobs, records) = self.slurm.jobs().await?;
                Answer::Jobs(self.listed(&jobs, &records).await?)
            }
            Call::Nodes => {
                let nodes = self.slurm.nodes().await?;
                Answer::Nodes(nodes.into_iter().map(node).collect::<Result<_>>()?)
            }
            Call::Partitions => {
                let partitions = self.slurm.partitions().await?;
                Answer::Partitions(
                    partitions
                        .into_iter()
                        .map(partition)
                        .collect::<Result<_>>()?,
                )
            }
            Call::Placements => Answer::Placements(if let Some(placements) = listed {
                placements?
            } else {
                let (_, records) = self.slurm.jobs().await?;
                records.iter().map(placed).collect::<Result<_>>()?
            }),
            Call::Lookup { key, .. } => Answer::Lookup(match self.slurm.job(key.job.0).await? {
                Some(found) => Lookup::Found(placed(&found)?),
                None => Lookup::Gone,
            }),
            Call::Outcomes(ids) => {
                let ids: Vec<u32> = ids.iter().map(|id| id.0).collect();
                let query = accounting::Query::Jobs(&ids);
                let (records, _) = self.slurm.accounting(&query, UnixTime::now().0).await?;
                Answer::Outcomes(readable(records.iter().map(outcome)))
            }
            Call::History { from, to } => {
                let window = accounting::Query::Window(from.0, to.0);
                let (records, tres) = self.slurm.accounting(&window, UnixTime::now().0).await?;
                self.learn_logins(records.iter().map(|record| record.user_id))
                    .await;
                Answer::History(readable(
                    records
                        .iter()
                        .map(|record| accounted(record, tres, &self.logins)),
                ))
            }
            Call::Stats => Answer::Stats(stats(self.slurm.stats().await?)),
        })
    }

    /// The jobs of a listing as the model lists them. Keeps the listing's placements for the
    /// placement call that follows.
    async fn listed(
        &mut self,
        jobs: &[jobs::Job],
        records: &[jobs::Placed],
    ) -> Result<Vec<Attempt>> {
        self.next_placements = Some(records.iter().map(placed).collect());
        self.learn_logins(jobs.iter().map(|job| job.user_id)).await;
        let now = UnixTime::now();
        // 25.11 leaves the submit time out of a job without details, and an attempt is keyed on it.
        jobs.iter()
            .filter(|job| job.submit != 0)
            .map(|job| attempt(job, now, &self.logins))
            .collect()
    }

    /// Looks up the logins of UIDs not seen before, as squeue would. A UID the account database lacks
    /// is not asked about again.
    async fn learn_logins(&mut self, uids: impl Iterator<Item = u32>) {
        let mut new: Vec<u32> = uids
            .filter(|uid| !self.logins.contains_key(uid) && !self.unknown.contains(uid))
            .collect();
        new.sort_unstable();
        new.dedup();
        if new.is_empty() {
            return;
        }
        match crate::passwd::logins(&new).await {
            Ok(found) => {
                self.unknown
                    .extend(new.iter().filter(|uid| !found.contains_key(uid)));
                self.logins.extend(found);
                self.looking_up_logins.worked("login lookup");
            }
            Err(error) => self.looking_up_logins.failed("login lookup", error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slurm::cli::Cli;
    use crate::slurm::rpc::controllers;

    #[test]
    fn an_unconvertible_record_is_skipped() {
        let converted = vec![Ok(1), Err(anyhow::anyhow!("bad TRES")), Ok(3)];
        assert_eq!(readable(converted.into_iter()), [1, 3]);
    }

    #[tokio::test]
    async fn placements_right_after_jobs_reuse_their_reply() {
        let dir = tempfile::tempdir().unwrap();
        let nowhere = Client::new(Vec::new(), Issuer::Sack(dir.path().join("no.socket")));
        let mut wire = Wire::over(nowhere);
        let none = Answer::Placements(Vec::new());
        wire.listed(&[], &[]).await.unwrap();
        // No controller can be reached, so only the listing can answer.
        assert_eq!(wire.answer(&Call::Placements).await.unwrap(), none);
        assert!(
            wire.answer(&Call::Placements).await.is_err(),
            "a reply serves one placement call only"
        );
        wire.listed(&[], &[]).await.unwrap();
        wire.answer(&Call::Partitions).await.ok();
        assert!(
            wire.answer(&Call::Placements).await.is_err(),
            "another call in between drops the reply"
        );
    }

    /// Both backends, connected. The live tests compare them on zlurm, where the cluster can change
    /// between the two answers.
    async fn backends() -> (Wire, Cli) {
        let mut wire = Wire::new(controllers().unwrap()).unwrap();
        let mut cli = Cli::default();
        let (Answer::Config(wired), Answer::Config(printed)) = (
            wire.answer(&Call::Config).await.unwrap(),
            cli.answer(&Call::Config).await.unwrap(),
        ) else {
            panic!("not a config");
        };
        assert_eq!(wired, printed);
        (wire, cli)
    }

    async fn both(call: &Call) -> (Answer, Answer) {
        let (mut wire, mut cli) = backends().await;
        let started = std::time::Instant::now();
        let wired = wire.answer(call).await.unwrap();
        eprintln!("{call:?} over the wire took {:?}", started.elapsed());
        let started = std::time::Instant::now();
        let printed = cli.answer(call).await.unwrap();
        eprintln!("{call:?} by command took {:?}", started.elapsed());
        (wired, printed)
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn nodes_and_partitions_match_the_commands() {
        let (wired, printed) = both(&Call::Partitions).await;
        assert_eq!(wired, printed);
        let (Answer::Nodes(mut wired), Answer::Nodes(mut printed)) = both(&Call::Nodes).await
        else {
            panic!("not nodes");
        };
        for node in wired.iter_mut().chain(&mut printed) {
            node.load = None;
            node.mem_free_mb = None;
        }
        assert_eq!(wired, printed);
    }

    fn by_id(jobs: &[Attempt]) -> HashMap<(JobId, Option<String>), &Attempt> {
        jobs.iter()
            .map(|job| {
                (
                    (job.key.job, job.array.as_ref().map(|a| a.spec.clone())),
                    job,
                )
            })
            .collect()
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn jobs_and_placements_match_the_commands() {
        let (Answer::Jobs(wired), Answer::Jobs(printed)) = both(&Call::Jobs).await else {
            panic!("not jobs");
        };
        let printed = by_id(&printed);
        let key = |job: &Attempt| (job.key.job, job.array.as_ref().map(|a| a.spec.clone()));
        let differing: Vec<_> = wired
            .iter()
            .filter(|job| printed.get(&key(job)) != Some(job))
            .collect();
        for job in differing.iter().take(3) {
            eprintln!("wire    {job:?}\ncommand {:?}\n", printed.get(&key(job)));
        }
        let differ = differing.len();
        eprintln!("{} jobs, {differ} differ", wired.len());
        assert!(differ * 100 <= wired.len());

        let (Answer::Placements(wired), Answer::Placements(printed)) =
            both(&Call::Placements).await
        else {
            panic!("not placements");
        };
        let printed: HashMap<JobId, _> = printed.iter().map(|p| (p.job, p)).collect();
        let differ = wired
            .iter()
            .filter(|p| printed.get(&p.job).is_some_and(|q| *q != *p))
            .count();
        eprintln!("{} placements, {differ} differ", wired.len());
        assert!(differ * 100 <= wired.len());
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn lookups_match_the_commands() {
        let (mut wire, mut cli) = backends().await;
        let Answer::Jobs(jobs) = wire.answer(&Call::Jobs).await.unwrap() else {
            panic!("not jobs");
        };
        let running = jobs
            .iter()
            .filter(|job| job.state == glurmo_model::JobState::Running);
        for job in running.take(5) {
            let call = Call::Lookup {
                key: job.key,
                printed: job.printed_id(),
            };
            assert_eq!(
                wire.answer(&call).await.unwrap(),
                cli.answer(&call).await.unwrap()
            );
        }
        let gone = Call::Lookup {
            key: glurmo_model::AttemptKey {
                job: JobId(1),
                submit: UnixTime(1),
            },
            printed: "1".into(),
        };
        assert_eq!(
            wire.answer(&gone).await.unwrap(),
            Answer::Lookup(Lookup::Gone)
        );
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn stats_match_sdiag() {
        let (Answer::Stats(wired), Answer::Stats(printed)) = both(&Call::Stats).await else {
            panic!("not stats");
        };
        assert_eq!(wired.jobs_submitted, printed.jobs_submitted);
        assert_eq!(wired.backfilled_jobs, printed.backfilled_jobs);
        let kinds = |stats: &glurmo_model::Stats| {
            let mut kinds: Vec<String> = stats.rpcs.iter().map(|rpc| rpc.kind.clone()).collect();
            kinds.sort();
            kinds
        };
        let mut wired_kinds = kinds(&wired);
        wired_kinds.retain(|kind| kind != "REQUEST_JOB_INFO_SINGLE");
        let mut printed_kinds = kinds(&printed);
        printed_kinds.retain(|kind| kind != "REQUEST_JOB_INFO_SINGLE");
        assert_eq!(wired_kinds, printed_kinds);
    }

    #[tokio::test]
    #[ignore = "needs a live slurmdbd"]
    async fn accounting_matches_sacct() {
        let now = UnixTime::now();
        let window = Call::History {
            from: UnixTime(now.0 - 3600),
            to: UnixTime(now.0 - 600),
        };
        let (Answer::History(mut wired), Answer::History(mut printed)) = both(&window).await else {
            panic!("not history");
        };
        let order = |a: &Attempt| (a.key, a.array.as_ref().map(|t| t.spec.clone()));
        wired.sort_by_key(order);
        printed.sort_by_key(order);
        assert_eq!(wired.len(), printed.len());
        let differ: Vec<_> = wired.iter().zip(&printed).filter(|(w, p)| w != p).collect();
        for (w, p) in differ.iter().take(3) {
            eprintln!("wire    {w:?}\ncommand {p:?}\n");
        }
        assert!(
            differ.is_empty(),
            "{} of {} differ",
            differ.len(),
            wired.len()
        );

        let ids = printed.iter().take(20).map(|a| a.key.job).collect();
        let (Answer::Outcomes(mut wired), Answer::Outcomes(mut printed)) =
            both(&Call::Outcomes(ids)).await
        else {
            panic!("not outcomes");
        };
        wired.sort_by_key(|o| o.key);
        printed.sort_by_key(|o| o.key);
        assert_eq!(wired, printed);
    }

    #[test]
    fn every_counter_reaches_the_model() {
        let counted = stats::Stats {
            server_threads: Some(11),
            agent_queue: Some(12),
            dbd_queue: Some(13),
            jobs_submitted: Some(14),
            jobs_started: Some(15),
            jobs_completed: Some(16),
            main_cycle_mean_us: Some(300),
            main_queue_last: Some(17),
            backfill_cycle_mean_us: Some(500),
            backfilled_jobs: Some(18),
            rpcs: vec![stats::RpcCount {
                kind: "REQUEST_PING".into(),
                count: 9,
                total_us: 300,
            }],
        };
        let expected = glurmo_model::Stats {
            server_threads: Some(11),
            agent_queue: Some(12),
            dbd_queue: Some(13),
            jobs_submitted: Some(14),
            jobs_started: Some(15),
            jobs_completed: Some(16),
            main_cycle_mean_us: Some(300),
            main_queue_last: Some(17),
            backfill_cycle_mean_us: Some(500),
            backfilled_jobs: Some(18),
            rpcs: vec![glurmo_model::RpcCount {
                kind: "REQUEST_PING".into(),
                count: 9,
                total_us: 300,
            }],
        };
        assert_eq!(stats(counted), expected);
    }

    #[test]
    fn every_node_value_reaches_the_model() {
        let listed = nodes::Node {
            name: "z1".into(),
            partitions: vec!["long".into(), "short".into()],
            state: "allocated+drain+reserved".into(),
            reason: Some("bad fan".into()),
            cpus: 16,
            cpus_alloc: 12,
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
        let expected = Node {
            name: NodeName("z1".into()),
            partitions: vec![PartitionName("long".into()), PartitionName("short".into())],
            state: glurmo_model::NodeState::Draining,
            flags: vec![glurmo_model::NodeFlag::Reserved],
            state_raw: "allocated+drain+reserved".into(),
            reason: Some("bad fan".into()),
            cpus: 16,
            cpus_alloc: 12,
            sockets: 2,
            cores: 8,
            load: Some(2.5),
            mem_mb: 64_000,
            mem_alloc_mb: 48_000,
            mem_free_mb: Some(30_000),
            gpus: 4,
            gpus_used: 1,
            gpu_model: Some("v100".into()),
            features: vec!["skylake".into(), "avx512".into()],
        };
        assert_eq!(node(listed.clone()).unwrap(), expected);
        let unparsed = nodes::Node {
            gres_used: "gpu:1(IDX:0".into(),
            ..listed
        };
        let error = format!("{:#}", node(unparsed).unwrap_err());
        assert!(error.starts_with("node z1: "), "{error}");
    }

    #[test]
    fn every_partition_value_reaches_the_model() {
        let hidden = nodes::Partition {
            name: "gpu".into(),
            nodes: "z[1-2]".into(),
            max_time_s: Some(7_200),
            default: false,
            hidden: true,
            state: "DRAIN",
        };
        let expected = Partition {
            name: PartitionName("gpu".into()),
            nodes: vec![NodeName("z1".into()), NodeName("z2".into())],
            max_time_s: Some(7_200),
            default: false,
            hidden: true,
            state_raw: "DRAIN".into(),
        };
        assert_eq!(partition(hidden.clone()).unwrap(), expected);
        let default = nodes::Partition {
            default: true,
            hidden: false,
            ..hidden
        };
        let default = partition(default).unwrap();
        assert!(default.default && !default.hidden);
    }

    #[test]
    fn every_setting_reaches_the_site() {
        let read = config::Config {
            cluster: "zlurm".into(),
            version: "23.11.4".into(),
            min_job_age_s: 300,
            private_data: vec!["jobs".into()],
            slurmctld_parameters: Some("rl_enable,rl_bucket_size=50".into()),
            slurm_uid: 450,
            accounting: Vec::new(),
            munge: None,
        };
        let expected = crate::slurm::Site {
            cluster: "zlurm".into(),
            version: crate::slurm::Version(23, 11, 4),
            min_job_age_s: 300,
            private_data: vec!["jobs".into()],
            rate_limit: Some(crate::slurm::RateLimit {
                bucket_size: 50,
                refill_rate: 2,
                refill_period_s: 1,
            }),
        };
        assert_eq!(site(read.clone()).unwrap(), expected);
        let unlimited = config::Config {
            slurmctld_parameters: None,
            ..read.clone()
        };
        assert_eq!(site(unlimited).unwrap().rate_limit, None);
        let unversioned = config::Config {
            version: "next".into(),
            ..read
        };
        assert!(site(unversioned).is_err());
    }

    /// A running job on two hosts, as the protocol lists it, with a distinct value in each field.
    fn running() -> jobs::Job {
        jobs::Job {
            id: 20,
            array: None,
            user_id: 1001,
            user_name: Some("root".into()),
            partition: "gpu,cpu".into(),
            state: "RUNNING",
            reason: "None".into(),
            cpus: 6,
            tres: "cpu=6,mem=3000M,node=2,billing=6,gres/gpu=1".into(),
            time_limit: 90,
            submit: 100,
            start: 200,
            end: 5_600,
            nodes: "n[1-2]".into(),
            name: "align".into(),
        }
    }

    fn otter() -> HashMap<u32, Login> {
        HashMap::from([(1001, Login("otter".into()))])
    }

    #[test]
    fn every_job_value_reaches_the_model() {
        let nodes = vec![NodeName("n1".into()), NodeName("n2".into())];
        let expected = Attempt {
            key: AttemptKey {
                job: JobId(20),
                submit: UnixTime(100),
            },
            array: None,
            login: Login("otter".into()),
            partitions: vec![PartitionName("gpu".into()), PartitionName("cpu".into())],
            state: glurmo_model::JobState::Running,
            state_raw: "RUNNING".into(),
            cpus: 6,
            mem_mb: Some(3_000),
            gpus: 1,
            time_limit_s: Some(5_400),
            start: Some(UnixTime(200)),
            end: Some(UnixTime(5_600)),
            nodes: nodes.clone(),
            reason: None,
            name: "align".into(),
        };
        assert_eq!(
            attempt(&running(), UnixTime(300), &otter()).unwrap(),
            expected
        );
        let held = |cores: &[u32], gres: &str, memory_mb| jobs::HostCells {
            cores: cores.to_vec(),
            gres: gres.into(),
            memory_mb,
        };
        let on_hosts = jobs::Placed {
            id: 20,
            start: 200,
            state: "RUNNING",
            hosts: "n[1-2]".into(),
            cells: vec![
                held(&[0, 1], "gpu:1(IDX:2)", 1_000),
                held(&[0, 1, 2, 7], "", 2_000),
            ],
        };
        let cells = |node: &NodeName, cores: &[u32], gpus: &[u32], mem_mb| NodeCells {
            node: node.clone(),
            cores: cores.to_vec(),
            unplaced: 0,
            gpus: gpus.to_vec(),
            mem_mb: Some(mem_mb),
        };
        let expected = Placed {
            job: JobId(20),
            start: Some(UnixTime(200)),
            state: glurmo_model::JobState::Running,
            placement: Placement(vec![
                cells(&nodes[0], &[0, 1], &[2], 1_000),
                cells(&nodes[1], &[0, 1, 2, 7], &[], 2_000),
            ]),
        };
        assert_eq!(placed(&on_hosts).unwrap(), expected);
        let nowhere = jobs::Placed {
            hosts: String::new(),
            cells: Vec::new(),
            ..on_hosts
        };
        assert_eq!(placed(&nowhere).unwrap().placement, Placement(Vec::new()));
    }

    #[test]
    fn an_unlimited_job_ends_only_once_its_end_has_passed() {
        let unlimited = jobs::Job {
            time_limit: INFINITE,
            ..running()
        };
        let listed = attempt(&unlimited, UnixTime(300), &otter()).unwrap();
        assert_eq!((listed.time_limit_s, listed.end), (None, None));
        let listed = attempt(&unlimited, UnixTime(5_600), &otter()).unwrap();
        assert_eq!(listed.end, Some(UnixTime(5_600)));
        let unset = jobs::Job {
            time_limit: NO_VAL,
            ..running()
        };
        let listed = attempt(&unset, UnixTime(300), &otter()).unwrap();
        assert_eq!(
            (listed.time_limit_s, listed.end),
            (None, Some(UnixTime(5_600)))
        );
    }

    #[test]
    fn an_owner_the_account_database_lacks_takes_the_controllers_name_then_the_uid() {
        let logins = HashMap::new();
        let named = attempt(&running(), UnixTime(300), &logins).unwrap();
        assert_eq!(named.login, Login("root".into()));
        let nameless = jobs::Job {
            user_name: None,
            ..running()
        };
        let listed = attempt(&nameless, UnixTime(300), &logins).unwrap();
        assert_eq!(listed.login, Login("1001".into()));
    }

    #[test]
    fn a_pending_job_counts_what_it_requested_and_says_why_it_waits() {
        let waiting = jobs::Job {
            state: "PENDING",
            tres: "cpu=2,mem=1G,node=1,gres/gpu=4".into(),
            reason: "Priority".into(),
            start: 0,
            end: 0,
            nodes: String::new(),
            array: Some(jobs::ArrayTasks {
                job: 10,
                spec: "3".into(),
                count: 1,
            }),
            ..running()
        };
        let listed = attempt(&waiting, UnixTime(300), &otter()).unwrap();
        assert_eq!(listed.state, glurmo_model::JobState::Pending);
        assert_eq!((listed.mem_mb, listed.gpus), (Some(1_024), 4));
        assert_eq!(listed.reason.as_deref(), Some("Priority"));
        assert_eq!((listed.start, listed.end), (None, None));
        assert_eq!(listed.nodes, Vec::new());
        let expected = ArrayTasks {
            job: JobId(10),
            spec: "3".into(),
            count: 1,
        };
        assert_eq!(listed.array, Some(expected));
        let unsubmitted = jobs::Job {
            submit: 0,
            ..waiting
        };
        assert!(attempt(&unsubmitted, UnixTime(300), &otter()).is_err());
    }

    /// An ended array task as accounting holds it, with a distinct value in each field glurmo reads.
    fn ended() -> accounting::Record {
        accounting::Record {
            id: 9,
            array_base: 5,
            array_task: 3,
            array_range: None,
            array_max_tasks: 0,
            user_id: 20_001,
            user: Some("heron".into()),
            partition: Some("long".into()),
            state: "COMPLETED",
            req_cpus: 2,
            tres_alloc: Some("1=8,2=16000,1001=2".into()),
            tres_req: Some("1=2,2=4000".into()),
            time_limit: INFINITE,
            submit: 1_000,
            start: 1_100,
            end: 1_900,
            nodes: Some("z[1-2]".into()),
        }
    }

    const GPU: accounting::Tres = accounting::Tres { gpu: Some(1001) };

    #[test]
    fn every_accounting_value_reaches_the_model() {
        let key = AttemptKey {
            job: JobId(9),
            submit: UnixTime(1_000),
        };
        let completed = glurmo_model::JobState::Ended(glurmo_model::EndState::Completed);
        let expected = Outcome {
            key,
            end: Some(UnixTime(1_900)),
            state: completed,
        };
        assert_eq!(outcome(&ended()).unwrap(), expected);
        let logins = HashMap::from([(20_001, Login("otter".into()))]);
        let expected = Attempt {
            key,
            array: Some(ArrayTasks {
                job: JobId(5),
                spec: "3".into(),
                count: 1,
            }),
            login: Login("heron".into()),
            partitions: vec![PartitionName("long".into())],
            state: completed,
            state_raw: "COMPLETED".into(),
            cpus: 8,
            mem_mb: Some(16_000),
            gpus: 2,
            time_limit_s: None,
            start: Some(UnixTime(1_100)),
            end: Some(UnixTime(1_900)),
            nodes: vec![NodeName("z1".into()), NodeName("z2".into())],
            reason: None,
            name: String::new(),
        };
        assert_eq!(accounted(&ended(), GPU, &logins).unwrap(), expected);
    }

    #[test]
    fn a_pending_record_counts_what_it_requested() {
        let record = accounting::Record {
            id: 7,
            array_base: 0,
            array_task: NO_VAL,
            user: None,
            partition: Some("4hours,long".into()),
            state: "PENDING",
            req_cpus: 4,
            tres_alloc: Some(String::new()),
            tres_req: Some("1=4,2=8000,1001=1".into()),
            time_limit: 60,
            submit: 1_794_000_000,
            start: 0,
            end: 0,
            nodes: None,
            ..ended()
        };
        let logins = HashMap::from([(20_001, Login("otter".into()))]);
        let attempt = accounted(&record, GPU, &logins).unwrap();
        assert_eq!(
            (attempt.cpus, attempt.mem_mb, attempt.gpus),
            (4, Some(8_000), 1)
        );
        assert_eq!(attempt.login, Login("otter".into()));
        assert_eq!(attempt.partitions.len(), 2);
        assert_eq!(attempt.time_limit_s, Some(3_600));
        assert_eq!((attempt.start, attempt.end), (None, None));
        assert_eq!(attempt.array, None);
    }

    #[test]
    fn a_never_started_job_has_no_start() {
        assert_eq!(accounted_time(0), None);
        assert_eq!(
            accounted_time(i64::from(NO_VAL)),
            None,
            "a job cancelled while pending"
        );
        assert_eq!(accounted_time(1_794_731_534), Some(UnixTime(1_794_731_534)));
    }
}
