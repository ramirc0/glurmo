//! What slurmdbd holds about jobs, read over its persistent connection as sacct reads it.

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::jobs::{ArrayTasks, array_tasks, hex_tasks, state_name};
use super::pack::{NO_VAL, Pack, Unpack};
use super::{GLOBAL_AUTH_KEY, Protocol, SLURMDBD_CONNECTION, Session, TIMEOUT, frame, read_frame};

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
///
/// # Examples
///
/// ```
/// use glurmo_rpc::accounting::Query;
///
/// let ids = [4242, 4243];
/// let attempts = Query::Jobs(&ids);
/// let hour = Query::Window(1_794_000_000, 1_794_003_600);
/// assert!(matches!(hour, Query::Window(from, to) if to - from == 3_600));
/// ```
pub enum Query<'a> {
    /// Every attempt of the jobs with these IDs, whenever they ran.
    Jobs(&'a [u32]),
    /// Every allocation pending or running between two times, the second exclusive, whoever owns it.
    Window(i64, i64),
}

/// One allocation as accounting holds it, reduced to what glurmo reads.
///
/// # Examples
///
/// ```
/// use glurmo_rpc::NO_VAL;
/// use glurmo_rpc::accounting::Record;
///
/// let record = Record {
///     id: 4242,
///     array_base: 0,
///     array_task: NO_VAL,
///     array_range: None,
///     array_max_tasks: 0,
///     user_id: 1001,
///     user: Some("otter".into()),
///     partition: Some("5days".into()),
///     state: "COMPLETED",
///     req_cpus: 8,
///     tres_alloc: Some("1=8,2=16000,1001=1".into()),
///     tres_req: Some("1=8,2=16000,1001=1".into()),
///     time_limit: 1_440,
///     submit: 1_794_000_000,
///     start: 1_794_000_100,
///     end: 1_794_003_700,
///     nodes: Some("z1001".into()),
/// };
/// assert_eq!(record.end - record.start, 3_600);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The job ID, unique to each array task.
    pub id: u32,
    /// The array's base job ID, or 0 outside an array.
    pub array_base: u32,
    /// The array task ID, or [`NO_VAL`] for none.
    pub array_task: u32,
    /// The tasks a pending array record stands for, as a hex mask.
    pub array_range: Option<String>,
    /// How many of the array's tasks may run at once, or 0 for no throttle.
    pub array_max_tasks: u32,
    /// The UID of its owner.
    pub user_id: u32,
    /// The owner's login, when accounting holds it.
    pub user: Option<String>,
    /// The partitions it may run in, separated by commas.
    pub partition: Option<String>,
    /// The state as sacct prints it, such as `COMPLETED`.
    pub state: &'static str,
    /// CPUs requested.
    pub req_cpus: u32,
    /// The TRES it held, as `id=count` pairs such as `1=8,2=16000`. See [`Tres::counts`].
    pub tres_alloc: Option<String>,
    /// The TRES it asked for, in the same form.
    pub tres_req: Option<String>,
    /// The time limit in minutes. [`INFINITE`](super::pack::INFINITE) is no limit, and
    /// [`NO_VAL`] is a limit not set.
    pub time_limit: u32,
    /// When it was submitted. 0 is none.
    pub submit: i64,
    /// When it started. 0 is none, and so is [`NO_VAL`], which slurmdbd stores for a job cancelled
    /// while pending.
    pub start: i64,
    /// When it ended. 0 is none.
    pub end: i64,
    /// Its nodes, as a hostlist such as `z[1001-1004]`.
    pub nodes: Option<String>,
}

/// Reads the allocations `query` selects from the slurmdbd at the first address that answers.
/// `now` closes an open window as sacct does.
///
/// # Errors
///
/// Fails when no address answers, slurmdbd refuses, or a reply is malformed.
pub(crate) async fn records(
    addresses: &[String],
    session: &Session,
    cluster: &str,
    query: &Query<'_>,
    now: i64,
) -> Result<(Vec<Record>, Tres)> {
    let mut failures = Vec::new();
    for address in addresses {
        let session = async {
            let mut stream = open(address, session, cluster).await?;
            let tres =
                Tres::decode(&call(&mut stream, GET_TRES, &tres_condition(), GOT_TRES).await?)?;
            let body = call(
                &mut stream,
                GET_JOBS,
                &job_condition(cluster, query, now, session.protocol),
                GOT_JOBS,
            )
            .await?;
            anyhow::Ok((decode(&body, session.protocol)?, tres))
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
async fn open(address: &str, session: &Session, cluster: &str) -> Result<TcpStream> {
    let mut stream = TcpStream::connect(address).await?;
    let mut body = Pack::default();
    body.u16(session.protocol.number())
        .str(Some(cluster))
        .u16(PERSIST_TYPE_DBD)
        .u16(0);
    let request = frame(
        session,
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

/// Every TRES, deleted ones included, as sacct asks for them. Old jobs can hold a TRES deleted since.
fn tres_condition() -> Vec<u8> {
    let mut body = Pack::default();
    body.u64(0)
        .u32(NO_VAL)
        .u32(NO_VAL)
        .u32(NO_VAL)
        .u32(NO_VAL)
        .u16(1);
    body.0
}

/// A `slurmdb_job_cond_t` as `sacct -X -D` builds it: allocations only, every requeued attempt,
/// on this cluster, for every user.
fn job_condition(cluster: &str, query: &Query, now: i64, protocol: Protocol) -> Vec<u8> {
    let (jobs, from, to): (&[u32], i64, i64) = match *query {
        Query::Jobs(jobs) => (jobs, 0, now + 1),
        Query::Window(from, to) => {
            let to = if to == from || to == now { to + 1 } else { to };
            (&[], from, to)
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
        for &job in jobs {
            if protocol == Protocol::V25_11 {
                body.u64(0);
            }
            body.u32(job)
                .u32(NO_VAL)
                .u32(NO_VAL)
                .u32(NO_VAL)
                .u32(NO_VAL);
            if protocol == Protocol::V25_11 {
                body.u32(NO_VAL);
            }
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
/// Fails when the body does not follow `protocol`'s layout, or holds steps.
pub(crate) fn decode(body: &[u8], protocol: Protocol) -> Result<Vec<Record>> {
    let u = &mut Unpack::new(body);
    let records = u.list(|u| record(u, protocol))?;
    let code = u.u32()?;
    u.finish()?;
    if code != 0 {
        bail!("slurmdbd error {code}");
    }
    Ok(records)
}

#[allow(
    clippy::too_many_lines,
    reason = "it walks Slurm's layout field by field"
)]
fn record(u: &mut Unpack, protocol: Protocol) -> Result<Record> {
    let v25_11 = protocol == Protocol::V25_11;
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
    if v25_11 {
        let _qos_req = u.str()?;
    }
    let req_cpus = u.u32()?;
    let _req_mem = u.u64()?;
    let _requid = u.u32()?;
    if v25_11 {
        let _restart_cnt = u.u16()?;
    }
    let _resv_name = u.str()?;
    let _resvid = u.u32()?;
    if v25_11 {
        let _resv_req = u.str()?;
    }
    let _script = u.str()?;
    let _show_full = u.u32()?;
    let start = u.time()?;
    let state = u.u32()?;
    let _state_reason_prev = u.u32()?;
    let steps = u.u32()?;
    if steps != 0 && steps != NO_VAL {
        bail!("job {id} came with {steps} steps despite the request for none");
    }
    if v25_11 {
        let _segment_size = u.u16()?;
        u.skip_strs(3)?;
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
        id,
        array_base,
        array_task,
        array_range,
        array_max_tasks,
        user_id,
        user,
        partition,
        state: state_name(state),
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
///
/// # Examples
///
/// ```
/// use glurmo_rpc::accounting::Tres;
///
/// let tres = Tres { gpu: Some(1001) };
/// assert_eq!(tres.counts(Some("1001=2"))?.gpus, 2);
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tres {
    /// The ID of `gres/gpu`, if the site defines it.
    pub gpu: Option<u32>,
}

impl Tres {
    /// Finds the ID of `gres/gpu` in a `DBD_GOT_TRES` body.
    ///
    /// # Errors
    ///
    /// Fails when the body does not follow Slurm 23.11's layout or carries an error code.
    pub(crate) fn decode(body: &[u8]) -> Result<Self> {
        let u = &mut Unpack::new(body);
        let records = u.list(|u| {
            let _alloc_secs = u.u64()?;
            let _count = u.u64()?;
            let id = u.u32()?;
            let name = u.str()?;
            let kind = u.str()?;
            Ok((id, kind, name))
        })?;
        let code = u.u32()?;
        u.finish()?;
        if code != 0 {
            bail!("slurmdbd error {code}");
        }
        let gpu = records.into_iter().find_map(|(id, kind, name)| {
            (kind.as_deref() == Some("gres") && name.as_deref() == Some("gpu")).then_some(id)
        });
        Ok(Self { gpu })
    }

    /// Reads a TRES string such as `1=8,2=16000,4=1,1001=2`.
    ///
    /// # Errors
    ///
    /// Fails on an entry that is not `id=count`.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_rpc::accounting::Tres;
    ///
    /// let held = Tres { gpu: Some(1001) }.counts(Some("1=8,2=16000,4=1,1001=2"))?;
    /// assert_eq!((held.cpus, held.mem_mb, held.gpus), (Some(8), Some(16_000), 2));
    /// assert_eq!(Tres { gpu: None }.counts(None)?.cpus, None);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn counts(self, text: Option<&str>) -> Result<Counts> {
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

/// The CPUs, memory and GPUs in a TRES string.
///
/// # Examples
///
/// ```
/// use glurmo_rpc::accounting::Counts;
///
/// let requested = Counts { cpus: Some(4), ..Counts::default() };
/// assert_eq!((requested.mem_mb, requested.gpus), (None, 0));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// CPUs, if the string counts them.
    pub cpus: Option<u64>,
    /// Memory in MB, if the string counts it.
    pub mem_mb: Option<u64>,
    /// GPUs of any type.
    pub gpus: u64,
}

impl Record {
    /// The array tasks the record stands for: a pending range, one task, or none.
    ///
    /// # Errors
    ///
    /// Fails when the range is not a hex mask.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_rpc::NO_VAL;
    /// use glurmo_rpc::accounting::Record;
    ///
    /// let pending = Record {
    ///     array_base: 5,
    ///     array_task: NO_VAL,
    ///     array_range: Some("0x0D".into()),
    ///     array_max_tasks: 2,
    /// #   id: 5, user_id: 1001, user: None, partition: None, state: "PENDING", req_cpus: 1,
    /// #   tres_alloc: None, tres_req: None, time_limit: 60, submit: 1_794_000_000, start: 0, end: 0,
    /// #   nodes: None,
    /// };
    /// let tasks = pending.array_tasks()?.unwrap();
    /// assert_eq!((tasks.job, tasks.spec.as_str(), tasks.count), (5, "0,2-3%2", 3));
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn array_tasks(&self) -> Result<Option<ArrayTasks>> {
        let range = self.array_range.as_deref().map(hex_tasks).transpose()?;
        Ok(array_tasks(
            self.array_base,
            self.array_task,
            range.as_ref(),
            self.array_max_tasks,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tres_strings_give_cpus_memory_and_gpus() {
        let counts = Tres { gpu: Some(1001) }
            .counts(Some("1=8,2=16000,4=1,1001=2"))
            .unwrap();
        assert_eq!(
            (counts.cpus, counts.mem_mb, counts.gpus),
            (Some(8), Some(16_000), 2)
        );
        assert!(Tres { gpu: None }.counts(Some("1:8")).is_err());
    }

    #[test]
    fn the_tres_reply_is_read_to_its_return_code() {
        let mut body = Pack::default();
        body.u32(2);
        body.u64(0).u64(0).u32(1).str(Some("cpu")).str(Some("cpu"));
        body.u64(0)
            .u64(0)
            .u32(1001)
            .str(Some("gpu"))
            .str(Some("gres"));
        body.u32(0);
        assert_eq!(Tres::decode(&body.0).unwrap(), Tres { gpu: Some(1001) });
        let mut longer = Pack(body.0.clone());
        longer.u16(0);
        assert!(Tres::decode(&longer.0).is_err());
        let mut refused = Pack(body.0[..body.0.len() - 4].to_vec());
        refused.u32(1);
        assert!(Tres::decode(&refused.0).is_err());
    }

    #[test]
    fn the_tres_request_includes_deleted_ones_as_sacct_does() {
        let body = tres_condition();
        assert_eq!(body[body.len() - 2..], 1_u16.to_be_bytes());
    }

    #[test]
    fn a_record_stands_for_one_task_a_pending_range_or_none() {
        let task = Record {
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
            tres_alloc: None,
            tres_req: None,
            time_limit: 60,
            submit: 1_000,
            start: 1_100,
            end: 1_900,
            nodes: None,
        };
        let tasks = |spec: &str, count| ArrayTasks {
            job: 5,
            spec: spec.into(),
            count,
        };
        assert_eq!(task.array_tasks().unwrap(), Some(tasks("3", 1)));
        let range = Record {
            array_task: NO_VAL,
            array_range: Some("0x0D".into()),
            array_max_tasks: 2,
            ..task.clone()
        };
        assert_eq!(range.array_tasks().unwrap(), Some(tasks("0,2-3%2", 3)));
        let alone = Record {
            array_task: NO_VAL,
            ..task.clone()
        };
        assert_eq!(alone.array_tasks().unwrap(), None);
        let unreadable = Record {
            array_range: Some("0xZZ".into()),
            ..task
        };
        assert!(unreadable.array_tasks().is_err());
    }

    /// The window a job condition asks for, as (from, to).
    fn asked(query: &Query, now: i64, protocol: Protocol) -> (i64, i64) {
        let body = job_condition("zlurm", query, now, protocol);
        let time = |back: usize| {
            let at = body.len() - back;
            i64::from_be_bytes(body[at..at + 8].try_into().unwrap())
        };
        (time(20), time(28))
    }

    #[test]
    fn a_window_that_is_empty_or_ends_now_takes_one_more_second_as_sacct_gives_it() {
        let (from, to) = (100, 200);
        for protocol in Protocol::NEWEST_FIRST {
            assert_eq!(asked(&Query::Window(from, to), 500, protocol), (100, 200));
            assert_eq!(asked(&Query::Window(from, from), 500, protocol), (100, 101));
            assert_eq!(asked(&Query::Window(from, to), 200, protocol), (100, 201));
            assert_eq!(asked(&Query::Jobs(&[7]), 500, protocol), (0, 501));
        }
    }
}
