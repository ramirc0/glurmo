//! What slurmdbd holds about jobs, read over its persistent connection as sacct reads it.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::jobs::{array_tasks, hex_tasks, state_name};
use super::pack::{INFINITE, NO_VAL, NO_VAL64, Pack, Unpack};
use super::{GLOBAL_AUTH_KEY, PROTOCOL, SLURMDBD_CONNECTION, TIMEOUT, frame, read_frame};
use crate::slurm::fields::job_state;
use crate::slurm::hostlist::expand_hosts;
use crate::slurm::sacct::Outcome;
use glurmo_model::{Attempt, AttemptKey, JobId, Login, PartitionName, UnixTime};

const PERSIST_INIT: u16 = 6500;
const PERSIST_RC: u16 = 1433;
const PERSIST_TYPE_DBD: u16 = 1;
const GET_JOBS: u16 = 1444;
const GOT_JOBS: u16 = 1421;
const GET_TRES: u16 = 1486;
const GOT_TRES: u16 = 1487;

const DUPLICATES: u32 = 1;
const NO_STEPS: u32 = 1 << 1;
const NO_TRUNCATION: u32 = 1 << 2;
const JOB_FLAGS_NOT_SET: u32 = 1;

const TRES_CPU: u32 = 1;
const TRES_MEM: u32 = 2;

/// Which allocations to read.
pub enum Query<'a> {
    /// Every attempt of these jobs, whenever they ran.
    Jobs(&'a [JobId]),
    /// Every allocation pending or running in `[from, to)`, whoever owns it.
    Window(UnixTime, UnixTime),
}

/// One allocation as accounting holds it, reduced to what glurmo reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    id: JobId,
    array_base: u32,
    array_task: u32,
    array_range: Option<String>,
    array_max_tasks: u32,
    user_id: u32,
    user: Option<String>,
    partition: Option<String>,
    state: u32,
    req_cpus: u32,
    tres_alloc: Option<String>,
    tres_req: Option<String>,
    time_limit: u32,
    submit: i64,
    start: i64,
    end: i64,
    nodes: Option<String>,
}

/// Reads the allocations `query` selects from the slurmdbd at the first address that answers.
/// `now` closes an open window as sacct does.
///
/// # Errors
///
/// Fails when no address answers, slurmdbd refuses, or a reply is malformed.
pub async fn records(
    addresses: &[String],
    munge: &Path,
    decoder: Option<u32>,
    cluster: &str,
    query: &Query<'_>,
    now: UnixTime,
) -> Result<(Vec<Record>, Tres)> {
    let mut failures = Vec::new();
    for address in addresses {
        let session = async {
            let mut stream = open(address, munge, decoder, cluster).await?;
            let tres =
                Tres::decode(&call(&mut stream, GET_TRES, &tres_condition(), GOT_TRES).await?)?;
            let body = call(
                &mut stream,
                GET_JOBS,
                &job_condition(cluster, query, now),
                GOT_JOBS,
            )
            .await?;
            anyhow::Ok((decode(&body)?, tres))
        };
        match tokio::time::timeout(TIMEOUT * 4, session).await {
            Ok(Ok(found)) => return Ok(found),
            Ok(Err(error)) => failures.push(format!("{address}: {error:#}")),
            Err(_) => failures.push(format!("{address}: no reply within {:?}", TIMEOUT * 4)),
        }
    }
    bail!("slurmdbd did not answer: {}", failures.join("; "))
}

/// Opens a persistent connection.
async fn open(
    address: &str,
    munge: &Path,
    decoder: Option<u32>,
    cluster: &str,
) -> Result<TcpStream> {
    let mut stream = TcpStream::connect(address).await?;
    let mut body = Pack::default();
    body.u16(PROTOCOL)
        .str(Some(cluster))
        .u16(PERSIST_TYPE_DBD)
        .u16(0);
    let request = frame(
        munge,
        decoder,
        PERSIST_INIT,
        GLOBAL_AUTH_KEY | SLURMDBD_CONNECTION,
        &body.0,
    )
    .await?;
    stream.write_all(&request).await?;
    let reply = read_frame(&mut stream).await?;
    let mut u = Unpack::new(&reply);
    if u.u16()? != PERSIST_RC {
        bail!("slurmdbd answered the connection with an unexpected message");
    }
    persist_rc(&mut u)?;
    Ok(stream)
}

/// Reads a `PERSIST_RC` body and turns a nonzero code into an error.
fn persist_rc(u: &mut Unpack) -> Result<()> {
    let comment = u.str()?;
    let _flags = u.u16()?;
    let code = u.u32()?;
    let _version = u.u16()?;
    if code != 0 {
        bail!("slurmdbd error {code}: {}", comment.unwrap_or_default());
    }
    Ok(())
}

/// Sends one message on a persistent connection and returns the body of the expected reply.
async fn call(stream: &mut TcpStream, kind: u16, body: &[u8], expected: u16) -> Result<Vec<u8>> {
    let mut message = Pack::default();
    message.u32(u32::try_from(body.len() + 2)?).u16(kind);
    message.0.extend(body);
    stream.write_all(&message.0).await?;
    let reply = read_frame(stream).await?;
    let mut u = Unpack::new(&reply);
    match u.u16()? {
        kind if kind == expected => Ok(reply[2..].to_vec()),
        PERSIST_RC => {
            persist_rc(&mut u)?;
            bail!("slurmdbd answered with a bare return code")
        }
        kind => bail!("slurmdbd answered with message {kind}"),
    }
}

/// Every TRES, deleted ones included.
fn tres_condition() -> Vec<u8> {
    let mut body = Pack::default();
    body.u64(NO_VAL64)
        .u32(NO_VAL)
        .u32(NO_VAL)
        .u32(NO_VAL)
        .u32(NO_VAL)
        .u16(0);
    body.0
}

/// A `slurmdb_job_cond_t` as `sacct -X -D` builds it: allocations only, every requeued attempt,
/// on this cluster, for every user.
fn job_condition(cluster: &str, query: &Query, now: UnixTime) -> Vec<u8> {
    let (jobs, from, to): (&[JobId], i64, i64) = match *query {
        Query::Jobs(jobs) => (jobs, 0, now.0 + 1),
        Query::Window(from, to) => {
            let to = if to == from || to == now {
                to.0 + 1
            } else {
                to.0
            };
            (&[], from.0, to)
        }
    };
    let mut body = Pack::default();
    let none = |body: &mut Pack| {
        body.u32(NO_VAL);
    };
    none(&mut body);
    none(&mut body);
    body.u32(1).str(Some(cluster));
    none(&mut body);
    body.u32(0)
        .u32(0)
        .u32(JOB_FLAGS_NOT_SET)
        .u32(0)
        .u32(DUPLICATES | NO_STEPS | NO_TRUNCATION);
    for _list in 0..3 {
        none(&mut body);
    }
    body.u32(0).u32(0);
    for _list in 0..5 {
        none(&mut body);
    }
    if jobs.is_empty() {
        none(&mut body);
    } else {
        body.u32(u32::try_from(jobs.len()).expect("a short list"));
        for job in jobs {
            body.u32(job.0)
                .u32(NO_VAL)
                .u32(NO_VAL)
                .u32(NO_VAL)
                .u32(NO_VAL);
        }
    }
    none(&mut body);
    body.u32(0).u32(0).time(to).time(from).str(None);
    none(&mut body);
    none(&mut body);
    body.0
}

/// Decodes a `DBD_GOT_JOBS` body.
///
/// # Errors
///
/// Fails when the body does not follow Slurm 23.11's layout, or holds steps.
pub fn decode(body: &[u8]) -> Result<Vec<Record>> {
    let u = &mut Unpack::new(body);
    let records = u.list(|u| record(u))?;
    let code = u.u32()?;
    if code != 0 {
        bail!("slurmdbd error {code}");
    }
    Ok(records)
}

#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field"
)]
fn record(u: &mut Unpack) -> Result<Record> {
    let _account = u.str()?;
    let _admin_comment = u.str()?;
    let _alloc_nodes = u.u32()?;
    let array_base = u.u32()?;
    let array_max_tasks = u.u32()?;
    let array_task = u.u32()?;
    let array_range = u.str()?;
    let _associd = u.u32()?;
    u.skip_strs(4)?;
    let _db_index = u.u64()?;
    let _derived_ec = u.u32()?;
    let _derived_es = u.str()?;
    let _elapsed = u.u32()?;
    let _eligible = u.time()?;
    let end = u.time()?;
    let _env = u.str()?;
    let _exitcode = u.u32()?;
    let _extra = u.str()?;
    let _failed_node = u.str()?;
    let _flags = u.u32()?;
    let _gid = u.u32()?;
    let id = u.u32()?;
    u.skip_strs(4)?;
    let nodes = u.str()?;
    let _het_job = (u.u32()?, u.u32()?);
    let partition = u.str()?;
    let _priority = u.u32()?;
    let _qosid = u.u32()?;
    let req_cpus = u.u32()?;
    let _req_mem = u.u64()?;
    let _requid = u.u32()?;
    let _resv_name = u.str()?;
    let _resvid = u.u32()?;
    let _script = u.str()?;
    let _show_full = u.u32()?;
    let start = u.time()?;
    let state = u.u32()?;
    let _state_reason_prev = u.u32()?;
    let steps = u.u32()?;
    if steps != 0 && steps != NO_VAL {
        bail!("job {id} came with {steps} steps despite the request for none");
    }
    let submit = u.time()?;
    let _submit_line = u.str()?;
    let _suspended = u.u32()?;
    let _system_comment = u.str()?;
    let _sys_cpu = (u.u64()?, u.u64()?);
    let time_limit = u.u32()?;
    let _tot_cpu = (u.u64()?, u.u64()?);
    let tres_alloc = u.str()?;
    let tres_req = u.str()?;
    let user_id = u.u32()?;
    let user = u.str()?;
    let _user_cpu = (u.u64()?, u.u64()?);
    let _wckey = u.str()?;
    let _wckeyid = u.u32()?;
    let _work_dir = u.str()?;
    Ok(Record {
        id: JobId(id),
        array_base,
        array_task,
        array_range,
        array_max_tasks,
        user_id,
        user,
        partition,
        state,
        req_cpus,
        tres_alloc,
        tres_req,
        time_limit,
        submit,
        start,
        end,
        nodes,
    })
}

/// The TRES IDs glurmo reads in accounting's `id=count` strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tres {
    gpu: Option<u32>,
}

impl Tres {
    /// Finds the ID of `gres/gpu` in a `DBD_GOT_TRES` body.
    ///
    /// # Errors
    ///
    /// Fails when the body does not follow Slurm 23.11's layout.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let u = &mut Unpack::new(body);
        let records = u.list(|u| {
            let _alloc_secs = u.u64()?;
            let _count = u.u64()?;
            let id = u.u32()?;
            let name = u.str()?;
            let kind = u.str()?;
            Ok((id, kind, name))
        })?;
        let gpu = records.into_iter().find_map(|(id, kind, name)| {
            (kind.as_deref() == Some("gres") && name.as_deref() == Some("gpu")).then_some(id)
        });
        Ok(Self { gpu })
    }

    /// Reads a TRES string such as `1=8,2=16000,4=1,1001=2`.
    fn counts(self, text: Option<&str>) -> Result<Counts> {
        let mut counts = Counts::default();
        for entry in text
            .unwrap_or_default()
            .split(',')
            .filter(|e| !e.is_empty())
        {
            let (id, count) = entry
                .split_once('=')
                .with_context(|| format!("bad TRES entry {entry:?}"))?;
            let (id, count): (u32, u64) = (id.parse()?, count.parse()?);
            match id {
                TRES_CPU => counts.cpus = Some(count),
                TRES_MEM => counts.mem_mb = Some(count),
                _ if Some(id) == self.gpu => counts.gpus = count,
                _ => {}
            }
        }
        Ok(counts)
    }
}

#[derive(Default)]
struct Counts {
    cpus: Option<u64>,
    mem_mb: Option<u64>,
    gpus: u64,
}

impl Record {
    /// The UID of the job's owner.
    pub fn user_id(&self) -> u32 {
        self.user_id
    }

    /// What `sacct -X -D -o JobIDRaw,Submit,End,State` reports.
    ///
    /// # Errors
    ///
    /// Fails when the state is unknown or the record has no submit time.
    pub fn outcome(&self) -> Result<Outcome> {
        Ok(Outcome {
            key: self.key()?,
            end: epoch(self.end),
            state: job_state(state_name(self.state))?,
        })
    }

    /// The allocation as sacct's history columns describe it. A pending job has no allocation yet,
    /// so its CPUs, memory and GPUs come from the request. Accounting keeps no reasons, and glurmo
    /// keeps no names from it. `logins` names an owner that accounting does not.
    ///
    /// # Errors
    ///
    /// Fails when the state, node list or TRES does not parse, or the record has no submit time.
    pub fn attempt(&self, tres: Tres, logins: &HashMap<u32, Login>) -> Result<Attempt> {
        let allocated = tres.counts(self.tres_alloc.as_deref())?;
        let (cpus, counts) = match allocated.cpus {
            Some(cpus) if cpus > 0 => (cpus, allocated),
            _ => (
                u64::from(self.req_cpus),
                tres.counts(self.tres_req.as_deref())?,
            ),
        };
        let state_raw = state_name(self.state).to_owned();
        let range = self.array_range.as_deref().map(hex_tasks).transpose()?;
        Ok(Attempt {
            key: self.key()?,
            array: array_tasks(
                self.array_base,
                self.array_task,
                range.as_ref(),
                self.array_max_tasks,
            ),
            login: match &self.user {
                Some(user) if !user.is_empty() => Login(user.clone()),
                _ => logins
                    .get(&self.user_id)
                    .cloned()
                    .unwrap_or_else(|| Login(self.user_id.to_string())),
            },
            partitions: self
                .partition
                .as_deref()
                .unwrap_or_default()
                .split(',')
                .map(|name| PartitionName(name.to_owned()))
                .collect(),
            state: job_state(&state_raw)?,
            state_raw,
            cpus: u32::try_from(cpus)?,
            mem_mb: counts.mem_mb,
            gpus: u32::try_from(counts.gpus)?,
            time_limit_s: (self.time_limit != INFINITE && self.time_limit != NO_VAL)
                .then(|| u64::from(self.time_limit) * 60),
            start: epoch(self.start),
            end: epoch(self.end),
            nodes: expand_hosts(self.nodes.as_deref().unwrap_or_default())?,
            reason: None,
            name: String::new(),
        })
    }

    fn key(&self) -> Result<AttemptKey> {
        Ok(AttemptKey {
            job: self.id,
            submit: epoch(self.submit).context("no submit time")?,
        })
    }
}

/// A time accounting holds. Zero means none, and so does `NO_VAL`, which slurmdbd stores as the start
/// of a job cancelled while pending.
fn epoch(time: i64) -> Option<UnixTime> {
    (time != 0 && time != i64::from(NO_VAL)).then_some(UnixTime(time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_never_started_job_has_no_start() {
        assert_eq!(epoch(0), None);
        assert_eq!(
            epoch(i64::from(NO_VAL)),
            None,
            "a job cancelled while pending"
        );
        assert_eq!(epoch(1_794_731_534), Some(UnixTime(1_794_731_534)));
    }
}
