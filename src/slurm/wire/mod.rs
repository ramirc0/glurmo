//! Slurm's RPC protocol, spoken directly to slurmctld and slurmdbd.

pub mod accounting;
pub mod config;
pub mod jobs;
pub mod munge;
pub mod nodes;
pub mod pack;
pub mod reasons;
pub mod rpcs;
pub mod sack;
pub mod stats;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::Lookup;
use super::fields::{gpu_count, gpu_indices, gpu_model, gres, job_state, node_state, tres};
use super::hostlist::expand_hosts;
use super::recording::{Answer, Call};
use super::sacct::Outcome;
use super::scontrol::Placed;
use crate::logging::Streak;
use glurmo_model::{
    ArrayTasks, Attempt, AttemptKey, JobId, Login, Node, NodeCells, NodeName, Partition,
    PartitionName, Placement, UnixTime,
};
use pack::{INFINITE, NO_VAL, Pack, Unpack};

/// A version of Slurm's protocol that glurmo decodes. A controller answers in the client's version
/// and, per Slurm's source, accepts clients up to three releases older than itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// Slurm 23.11's, which controllers from 23.11 through 25.05 accept.
    V23_11,
    /// Slurm 25.11's, which controllers from 25.11 through 27.05 accept.
    V25_11,
}

impl Protocol {
    /// Every version glurmo speaks, newest first, as it tries them.
    pub const NEWEST_FIRST: [Self; 2] = [Self::V25_11, Self::V23_11];

    /// The version as Slurm numbers it.
    pub fn number(self) -> u16 {
        match self {
            Self::V23_11 => 40 << 8,
            Self::V25_11 => 44 << 8,
        }
    }
}

/// Where glurmo gets the credential that signs each request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Issuer {
    /// munged, for `auth/munge`.
    Munge(PathBuf),
    /// slurmd's or sackd's sack socket, for `auth/slurm`.
    Sack(PathBuf),
}

impl Issuer {
    /// munged when it runs here, else a sack socket.
    ///
    /// # Errors
    ///
    /// Fails when neither runs.
    pub fn find() -> Result<Self> {
        let munge = PathBuf::from(munge::SOCKET);
        if munge.exists() {
            return Ok(Self::Munge(munge));
        }
        sack::socket().map(Self::Sack).with_context(|| {
            format!(
                "neither munged ({}) nor a sack socket ({}) runs here",
                munge::SOCKET,
                sack::SOCKETS.join(", ")
            )
        })
    }

    /// A credential for a message whose hash payload is `payload`, as the auth plugin's ID and token.
    async fn credential(
        &self,
        decoder: Option<u32>,
        payload: &[u8],
        protocol: Protocol,
    ) -> Result<(u32, String)> {
        match self {
            Self::Munge(socket) => Ok((
                AUTH_MUNGE,
                munge::credential(socket, decoder, payload).await?,
            )),
            Self::Sack(socket) => Ok((
                AUTH_SLURM,
                sack::token(socket, protocol.number(), decoder, payload).await?,
            )),
        }
    }
}

/// How glurmo signs its requests and which protocol it speaks to one cluster.
#[derive(Clone, Debug)]
pub struct Session {
    /// Where credentials come from.
    pub issuer: Issuer,
    /// The UID that may decode our credentials to slurmctld: `SlurmUser`, once known.
    pub decoder: Option<u32>,
    /// The protocol version.
    pub protocol: Protocol,
}

const AUTH_MUNGE: u32 = 101;
const AUTH_SLURM: u32 = 103;
const HASH_NONE: u8 = 1;
const NO_AUTH_CRED: u16 = 1 << 6;
const GLOBAL_AUTH_KEY: u16 = 1;
const SLURMDBD_CONNECTION: u16 = 1 << 1;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;
const RESPONSE_SLURM_RC: u16 = 8001;
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MESSAGE: usize = 1 << 30;

/// Where slurmctld listens and how to prove who we are to it.
pub struct Controller {
    /// `host:port` addresses to try in order: the primary, then backups. One in standby passes the call
    /// on, as it does for libslurm.
    pub addresses: Vec<String>,
    /// How requests are signed, and in which protocol.
    pub session: Session,
    /// The index of the address that answered last, which each call tries first, so a dead primary
    /// costs one timeout and not one per call.
    answered: AtomicUsize,
}

impl Controller {
    /// Sends one request and returns the body of its reply, which must be of type `response`.
    ///
    /// # Errors
    ///
    /// Fails when no address answers, no credential can be had, the reply is malformed or of another
    /// type, or the controller returns an error code.
    pub async fn call(&self, request: u16, body: &[u8], response: u16) -> Result<Vec<u8>> {
        let request = frame(&self.session, request, 0, body).await?;
        let first = self.answered.load(Ordering::Relaxed);
        let mut failures = Vec::new();
        for i in (0..self.addresses.len()).map(|n| (first + n) % self.addresses.len()) {
            let address = &self.addresses[i];
            match tokio::time::timeout(TIMEOUT, exchange(address, &request)).await {
                Ok(Ok(reply)) => match checked(&reply, response, self.session.protocol) {
                    Err(error)
                        if matches!(
                            error.downcast_ref::<ReturnCode>(),
                            Some(&(IN_STANDBY_MODE | IN_STANDBY_USE_BACKUP))
                        ) =>
                    {
                        failures.push(format!("{address}: in standby"));
                    }
                    checked => {
                        if self.answered.swap(i, Ordering::Relaxed) != i {
                            tracing::info!("slurmctld at {address} answers now");
                        }
                        return checked;
                    }
                },
                Ok(Err(error)) => failures.push(format!("{address}: {error:#}")),
                Err(_) => failures.push(format!("{address}: no reply within {TIMEOUT:?}")),
            }
        }
        bail!("slurmctld did not answer: {}", failures.join("; "))
    }
}

/// Frames a request: its length, a header, a credential and the body. A slurmdbd connection puts the
/// message type before the header too, and its credential is for any UID, as libslurm signs it,
/// because slurmdbd can run as another user than slurmctld's `SlurmUser`.
async fn frame(session: &Session, kind: u16, flags: u16, body: &[u8]) -> Result<Vec<u8>> {
    let mut hash = vec![HASH_NONE];
    hash.extend(kind.to_be_bytes());
    let decoder = session.decoder.filter(|_| flags & SLURMDBD_CONNECTION == 0);
    let credential = session.issuer.credential(decoder, &hash, session.protocol);
    let (plugin, credential) = tokio::time::timeout(TIMEOUT, credential)
        .await
        .with_context(|| format!("no credential from {:?} within {TIMEOUT:?}", session.issuer))??;
    let mut header = Pack::default();
    if flags & SLURMDBD_CONNECTION != 0 {
        header.u16(kind);
    }
    header
        .u16(session.protocol.number())
        .u16(flags)
        .u16(kind)
        .u32(u32::try_from(body.len())?)
        .u16(0)
        .u16(0)
        .u16(0);
    let mut auth = Pack::default();
    auth.u32(plugin).str(Some(&credential));
    let len = header.0.len() + auth.0.len() + body.len();
    let mut frame = Pack::default();
    frame.u32(u32::try_from(len)?);
    frame.0.extend(header.0);
    frame.0.extend(auth.0);
    frame.0.extend(body);
    Ok(frame.0)
}

async fn exchange(address: &str, request: &[u8]) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect(address).await?;
    stream.write_all(request).await?;
    read_frame(&mut stream).await
}

/// Reads a length-prefixed message.
async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let len = stream.read_u32().await? as usize;
    if len > MAX_MESSAGE {
        bail!("a {len}-byte reply is too large");
    }
    let mut reply = vec![0; len];
    stream.read_exact(&mut reply).await?;
    Ok(reply)
}

/// Strips the header and credential, and turns a nonzero return code or a reply of another type than
/// `response` into an error.
fn checked(frame: &[u8], response: u16, protocol: Protocol) -> Result<Vec<u8>> {
    let mut unpack = Unpack::new(frame);
    let version = unpack.u16()?;
    if version != protocol.number() {
        bail!(
            "slurmctld replied in protocol {version:#x}, not {:#x}",
            protocol.number()
        );
    }
    let flags = unpack.u16()?;
    let kind = unpack.u16()?;
    let body_len = unpack.u32()? as usize;
    if unpack.u16()? != 0 || unpack.u16()? != 0 {
        bail!("slurmctld sent a forwarded reply");
    }
    match unpack.u16()? {
        AF_INET => {
            unpack.take(6)?;
        }
        AF_INET6 => {
            unpack.mem()?;
            unpack.u16()?;
        }
        _ => {}
    }
    if flags & NO_AUTH_CRED == 0 {
        unpack.u32()?;
        unpack.skip_str()?;
    }
    if unpack.remaining() != body_len {
        bail!(
            "header says {body_len} body bytes, {} remain",
            unpack.remaining()
        );
    }
    let body = unpack.take(body_len)?.to_vec();
    if kind == RESPONSE_SLURM_RC {
        let code = Unpack::new(&body).u32().context("empty return code")?;
        if code != 0 {
            return Err(ReturnCode(code).into());
        }
    }
    if kind != response {
        bail!("slurmctld answered with message {kind}, not {response}");
    }
    Ok(body)
}

/// A nonzero code from a `RESPONSE_SLURM_RC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReturnCode(pub u32);

/// The code for a job the controller does not know.
pub const INVALID_JOB_ID: ReturnCode = ReturnCode(2017);

/// The code a backup controller in standby answers with, `ESLURM_IN_STANDBY_MODE`.
const IN_STANDBY_MODE: ReturnCode = ReturnCode(2027);

/// The code a backup in standby answers with when the primary should be asked, `ESLURM_IN_STANDBY_USE_BACKUP`.
const IN_STANDBY_USE_BACKUP: ReturnCode = ReturnCode(2131);

impl std::fmt::Display for ReturnCode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Slurm error {}", self.0)
    }
}

impl std::error::Error for ReturnCode {}

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

/// The paths libslurm tries for slurm.conf, after `$SLURM_CONF`. The last is slurmd's cache on a
/// configless cluster.
const SLURM_CONF: [&str; 2] = ["/etc/slurm/slurm.conf", "/run/slurm/conf/slurm.conf"];

const DEFAULT_PORT: u16 = 6817;

/// Finds slurmctld's addresses, primary first, in slurm.conf. Nothing else is read from it: the rest
/// comes from the controller.
///
/// # Errors
///
/// Fails when no slurm.conf can be read or it names no controller.
pub fn controllers() -> Result<Vec<String>> {
    let path = std::env::var_os("SLURM_CONF")
        .map(PathBuf::from)
        .into_iter()
        .chain(SLURM_CONF.iter().map(PathBuf::from))
        .find(|path| path.is_file())
        .context("no slurm.conf: set SLURM_CONF or name the controller in the config")?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let addresses = controllers_in(&text);
    if addresses.is_empty() {
        bail!("{} names no SlurmctldHost", path.display());
    }
    Ok(addresses)
}

/// Reads `SlurmctldHost=name(address)` lines, or the older `ControlMachine` and `BackupController`,
/// with `SlurmctldPort`.
fn controllers_in(conf: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut port = DEFAULT_PORT;
    for line in conf.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "slurmctldhost" | "controlmachine" | "backupcontroller" => {
                let host = value
                    .split_once('(')
                    .map_or(value, |(_, address)| address.trim_end_matches(')'));
                hosts.push(host.to_owned());
            }
            "slurmctldport" => {
                port = value
                    .split('-')
                    .next()
                    .and_then(|first| first.parse().ok())
                    .unwrap_or(DEFAULT_PORT);
            }
            _ => {}
        }
    }
    hosts
        .into_iter()
        .map(|host| format!("{host}:{port}"))
        .collect()
}

/// A client for one cluster's slurmctld and slurmdbd. Every call opens its own connection.
///
/// [`connect`](Self::connect) comes first, because the configuration says who may decode the
/// client's credentials and where slurmdbd listens. Until then, calls go out in the newest protocol
/// and anyone may decode their credentials.
pub struct Client {
    controller: Controller,
    /// `ClusterName`, which a slurmdbd connection names.
    cluster: String,
    /// slurmdbd's addresses, primary first.
    accounting: Vec<String>,
}

impl Client {
    /// A client for the slurmctld at `addresses`, primary first, whose requests `issuer` signs.
    pub fn new(addresses: Vec<String>, issuer: Issuer) -> Self {
        Self {
            controller: Controller {
                addresses,
                session: Session {
                    issuer,
                    decoder: None,
                    protocol: Protocol::NEWEST_FIRST[0],
                },
                answered: AtomicUsize::new(0),
            },
            cluster: String::new(),
            accounting: Vec::new(),
        }
    }

    /// Reads the configuration in the newest protocol the controller accepts, and keeps that
    /// protocol. From here on, only `SlurmUser` may decode the client's credentials.
    ///
    /// # Errors
    ///
    /// Fails when the controller answers in none of the protocols, or the configuration lacks the
    /// cluster's name or the Slurm version.
    pub async fn connect(&mut self) -> Result<config::Config> {
        let mut failures = Vec::new();
        for protocol in Protocol::NEWEST_FIRST {
            self.controller.session.protocol = protocol;
            match self.config().await {
                Ok(config) => {
                    tracing::info!(
                        "slurmctld at {} speaks protocol {protocol:?}, signed through {:?}; slurmdbd at {}",
                        self.controller.addresses.join(", "),
                        self.controller.session.issuer,
                        self.accounting.join(", ")
                    );
                    return Ok(config);
                }
                Err(error) => failures.push(format!("{protocol:?}: {error:#}")),
            }
        }
        bail!("no protocol glurmo speaks worked: {}", failures.join("; "))
    }

    async fn config(&mut self) -> Result<config::Config> {
        let mut body = Pack::default();
        body.time(0);
        let protocol = self.controller.session.protocol;
        let reply = self
            .controller
            .call(config::REQUEST, &body.0, config::RESPONSE)
            .await?;
        let config = config::decode(&reply, protocol)?;
        let session = &mut self.controller.session;
        session.decoder = Some(config.slurm_uid);
        if let (Issuer::Munge(socket), Some(named)) = (&mut session.issuer, &config.munge) {
            socket.clone_from(named);
        }
        self.cluster.clone_from(&config.cluster);
        self.accounting.clone_from(&config.accounting);
        Ok(config)
    }

    /// Two views of one reply that holds every job. The first lists the jobs as squeue does, with
    /// pending array tasks folded into their array's record. The second holds the cores of every
    /// record, as `scontrol -d show job` reports them.
    ///
    /// # Errors
    ///
    /// Fails when slurmctld does not answer, refuses, or sends a reply that does not decode.
    pub async fn jobs(&self) -> Result<(Vec<jobs::Job>, Vec<jobs::Placed>)> {
        let reply = self
            .controller
            .call(jobs::REQUEST, &jobs::request(), jobs::RESPONSE)
            .await?;
        jobs::decode(&reply, self.controller.session.protocol)
    }

    /// The cores of the job with this ID, or `None` when the controller no longer knows the job.
    ///
    /// # Errors
    ///
    /// Fails when slurmctld does not answer, refuses for another reason, or sends a reply that does
    /// not decode.
    pub async fn job(&self, id: u32) -> Result<Option<jobs::Placed>> {
        let protocol = self.controller.session.protocol;
        let request = jobs::single_request(id, protocol);
        let reply = self
            .controller
            .call(jobs::SINGLE_REQUEST, &request, jobs::RESPONSE)
            .await;
        match reply {
            Ok(reply) => {
                let (_, placed) = jobs::decode(&reply, protocol)?;
                Ok(placed.into_iter().find(|job| job.id == id))
            }
            Err(error) if error.downcast_ref::<ReturnCode>() == Some(&INVALID_JOB_ID) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Every node with its partitions, as sinfo lists them. Like sinfo, it leaves out nodes in no
    /// partition.
    ///
    /// # Errors
    ///
    /// Fails when slurmctld does not answer, refuses, or sends a reply that does not decode.
    pub async fn nodes(&self) -> Result<Vec<nodes::Node>> {
        let partitions = self.partition_records().await?;
        let reply = self
            .controller
            .call(
                nodes::NODE_REQUEST,
                &nodes::node_request(),
                nodes::NODE_RESPONSE,
            )
            .await?;
        nodes::nodes(&reply, &partitions, self.controller.session.protocol)
    }

    /// Every partition, hidden ones included.
    ///
    /// # Errors
    ///
    /// Fails when slurmctld does not answer, refuses, or sends a reply that does not decode.
    pub async fn partitions(&self) -> Result<Vec<nodes::Partition>> {
        let records = self.partition_records().await?;
        Ok(records.into_iter().map(|record| record.partition).collect())
    }

    async fn partition_records(&self) -> Result<Vec<nodes::PartitionRecord>> {
        let reply = self
            .controller
            .call(
                nodes::PARTITION_REQUEST,
                &nodes::partition_request(),
                nodes::PARTITION_RESPONSE,
            )
            .await?;
        nodes::partitions(&reply, self.controller.session.protocol)
    }

    /// Scheduler and RPC counters, as sdiag prints them.
    ///
    /// # Errors
    ///
    /// Fails when slurmctld does not answer, refuses, or sends a reply that does not decode.
    pub async fn stats(&self) -> Result<stats::Stats> {
        let reply = self
            .controller
            .call(stats::REQUEST, &stats::request(), stats::RESPONSE)
            .await?;
        stats::decode(&reply, self.controller.session.protocol)
    }

    /// The allocations `query` selects in slurmdbd, with the TRES IDs that read their TRES strings.
    /// `now` is the current time in epoch seconds, which closes an open window as sacct does.
    ///
    /// # Errors
    ///
    /// Fails when the site keeps no accounting in slurmdbd, or slurmdbd does not answer, refuses, or
    /// sends a reply that does not decode.
    pub async fn accounting(
        &self,
        query: &accounting::Query<'_>,
        now: i64,
    ) -> Result<(Vec<accounting::Record>, accounting::Tres)> {
        if self.accounting.is_empty() {
            bail!("the site keeps no accounting in slurmdbd");
        }
        accounting::records(
            &self.accounting,
            &self.controller.session,
            &self.cluster,
            query,
            now,
        )
        .await
    }
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
    use glurmo_model::{Attempt, JobId};

    #[test]
    fn an_unconvertible_record_is_skipped() {
        let converted = vec![Ok(1), Err(anyhow::anyhow!("bad TRES")), Ok(3)];
        assert_eq!(readable(converted.into_iter()), [1, 3]);
    }

    /// A reply frame without its length prefix, as `exchange` returns it, with a munge credential.
    fn reply(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut frame = Pack::default();
        frame
            .u16(Protocol::V23_11.number())
            .u16(0)
            .u16(kind)
            .u32(u32::try_from(body.len()).unwrap())
            .u16(0)
            .u16(0)
            .u16(0)
            .u32(AUTH_MUNGE)
            .str(Some("MUNGE:credential"));
        frame.0.extend(body);
        frame.0
    }

    #[test]
    fn replies_give_their_body_or_their_return_code() {
        let ok = reply(2008, b"body");
        assert_eq!(checked(&ok, 2008, Protocol::V23_11).unwrap(), b"body");
        assert!(checked(&ok, 2009, Protocol::V23_11).is_err());
        assert!(checked(&ok, 2008, Protocol::V25_11).is_err());
        let mut code = Pack::default();
        code.u32(2017);
        let refused = checked(&reply(RESPONSE_SLURM_RC, &code.0), 2008, Protocol::V23_11);
        assert_eq!(
            refused.unwrap_err().downcast_ref::<ReturnCode>(),
            Some(&INVALID_JOB_ID)
        );
        for len in 0..ok.len() {
            assert!(
                checked(&ok[..len], 2008, Protocol::V23_11).is_err(),
                "{len} bytes"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_credential_daemon_that_never_answers_fails_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("munge.socket");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        for issuer in [Issuer::Munge(socket.clone()), Issuer::Sack(socket.clone())] {
            let controller = Controller {
                addresses: Vec::new(),
                session: Session {
                    issuer,
                    decoder: None,
                    protocol: Protocol::V23_11,
                },
                answered: AtomicUsize::new(0),
            };
            let call = controller.call(1, &[], 2);
            let outcome = tokio::time::timeout(Duration::from_secs(3_600), call).await;
            assert!(outcome.is_ok_and(|reply| reply.is_err()));
        }
    }

    /// A sack socket that signs one request and reports the UID it was asked to sign for.
    fn sack_once(socket: &std::path::Path) -> tokio::task::JoinHandle<u32> {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _protocol = stream.read_u16().await.unwrap();
            let len = stream.read_u32().await.unwrap() as usize;
            let mut rest = vec![0; len - 4];
            stream.read_exact(&mut rest).await.unwrap();
            let decoder = Unpack::new(&rest[4..]).u32().unwrap();
            stream.write_u32(4).await.unwrap();
            stream.write_all(b"a.b\0").await.unwrap();
            decoder
        })
    }

    #[tokio::test]
    async fn slurmdbd_credentials_are_for_any_uid_as_libslurm_signs_them() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sack.socket");
        let session = Session {
            issuer: Issuer::Sack(socket.clone()),
            decoder: Some(500),
            protocol: Protocol::V23_11,
        };
        let signed = sack_once(&socket);
        frame(&session, 2007, 0, &[]).await.unwrap();
        assert_eq!(signed.await.unwrap(), 500);
        std::fs::remove_file(&socket).unwrap();
        let signed = sack_once(&socket);
        frame(&session, 6500, GLOBAL_AUTH_KEY | SLURMDBD_CONNECTION, &[])
            .await
            .unwrap();
        assert_eq!(signed.await.unwrap(), u32::MAX);
    }

    /// A sack socket that signs every request.
    fn sack_forever(socket: &std::path::Path) {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _protocol = stream.read_u16().await.unwrap();
                let len = stream.read_u32().await.unwrap() as usize;
                let mut rest = vec![0; len - 4];
                stream.read_exact(&mut rest).await.unwrap();
                stream.write_u32(4).await.unwrap();
                stream.write_all(b"a.b\0").await.unwrap();
            }
        });
    }

    /// A slurmctld that answers every request with `body` as message `kind`, and counts requests.
    async fn controller_answering(
        kind: u16,
        body: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let len = stream.read_u32().await.unwrap() as usize;
                let mut request = vec![0; len];
                stream.read_exact(&mut request).await.unwrap();
                let frame = reply(kind, &body);
                stream
                    .write_u32(u32::try_from(frame.len()).unwrap())
                    .await
                    .unwrap();
                stream.write_all(&frame).await.unwrap();
            }
        });
        (address, requests)
    }

    /// A client for 23.11 controllers at `addresses`, signing through the sack socket at `sack`.
    fn client_to(addresses: Vec<String>, sack: PathBuf) -> Client {
        let mut client = Client::new(addresses, Issuer::Sack(sack));
        client.controller.session.protocol = Protocol::V23_11;
        client
    }

    /// A slurmctld that hangs up on every request, and counts them.
    async fn controller_hanging_up() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            loop {
                let connection = listener.accept().await.unwrap();
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(connection);
            }
        });
        (address, requests)
    }

    #[tokio::test]
    async fn calls_go_first_to_the_controller_that_answered_last() {
        let dir = tempfile::tempdir().unwrap();
        let sack = dir.path().join("sack.socket");
        sack_forever(&sack);
        let (primary, refused) = controller_hanging_up().await;
        let mut no_jobs = Pack::default();
        no_jobs.u32(0).time(0).time(0);
        let (backup, answered) = controller_answering(jobs::RESPONSE, no_jobs.0).await;
        let client = client_to(vec![primary, backup], sack);
        let count = |requests: &std::sync::atomic::AtomicUsize| {
            requests.load(std::sync::atomic::Ordering::SeqCst)
        };
        client.jobs().await.unwrap();
        assert_eq!((count(&refused), count(&answered)), (1, 1));
        client.jobs().await.unwrap();
        client.jobs().await.unwrap();
        assert_eq!((count(&refused), count(&answered)), (1, 3));
    }

    #[tokio::test]
    async fn a_controller_in_standby_passes_calls_on_to_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let sack = dir.path().join("sack.socket");
        sack_forever(&sack);
        let mut in_standby = Pack::default();
        in_standby.u32(IN_STANDBY_MODE.0);
        let (backup, standing_by) = controller_answering(RESPONSE_SLURM_RC, in_standby.0).await;
        let mut no_jobs = Pack::default();
        no_jobs.u32(0).time(0).time(0);
        let (primary, answered) = controller_answering(jobs::RESPONSE, no_jobs.0).await;
        // The backup answered last, as after a failover, and the primary is back.
        let client = client_to(vec![backup, primary], sack);
        let count = |requests: &std::sync::atomic::AtomicUsize| {
            requests.load(std::sync::atomic::Ordering::SeqCst)
        };
        client.jobs().await.unwrap();
        assert_eq!((count(&standing_by), count(&answered)), (1, 1));
        client.jobs().await.unwrap();
        assert_eq!((count(&standing_by), count(&answered)), (1, 2));
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

    #[test]
    fn controllers_come_from_slurmctld_host_lines() {
        let conf = "# comment\nSlurmctldHost=ctl-1(192.0.2.11)\nslurmctldhost=backup\nSlurmctldPort=6820-6821\n";
        assert_eq!(controllers_in(conf), ["192.0.2.11:6820", "backup:6820"]);
        assert_eq!(controllers_in("ControlMachine=ctl\n"), ["ctl:6817"]);
        assert_eq!(controllers_in("ClusterName=x\n"), Vec::<String>::new());
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

    #[tokio::test]
    async fn a_job_the_controller_forgot_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let sack = dir.path().join("sack.socket");
        sack_forever(&sack);
        let mut invalid_job = Pack::default();
        invalid_job.u32(INVALID_JOB_ID.0);
        let (address, _) = controller_answering(RESPONSE_SLURM_RC, invalid_job.0).await;
        let client = client_to(vec![address], sack);
        assert_eq!(client.job(7).await.unwrap(), None);
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
