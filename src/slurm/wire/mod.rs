//! Slurm's RPC protocol, spoken directly to slurmctld and slurmdbd.

pub mod accounting;
pub mod config;
pub mod jobs;
pub mod munge;
pub mod nodes;
pub mod pack;
pub mod reasons;
pub mod rpcs;
pub mod stats;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use pack::{Pack, Unpack};

/// The protocol version glurmo speaks: Slurm 23.11's. Controllers accept it through 25.05.
pub const PROTOCOL: u16 = 40 << 8;

const AUTH_MUNGE: u32 = 101;
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
    /// `host:port` addresses to try in order: the primary, then backups.
    pub addresses: Vec<String>,
    /// munged's socket.
    pub munge: PathBuf,
    /// The UID that may decode our credentials: `SlurmUser`, once known.
    pub decoder: Option<u32>,
}

/// A reply's message type and body.
pub struct Reply {
    /// The message type.
    pub kind: u16,
    /// The body, after the header and credential.
    pub body: Vec<u8>,
}

impl Controller {
    /// Sends one request and reads its reply. A `RESPONSE_SLURM_RC` with a nonzero code is an error.
    ///
    /// # Errors
    ///
    /// Fails when no address answers, munged refuses, the reply is malformed, or the controller
    /// returns an error code.
    pub async fn call(&self, kind: u16, body: &[u8]) -> Result<Reply> {
        let request = frame(&self.munge, self.decoder, kind, 0, body).await?;
        let mut failures = Vec::new();
        for address in &self.addresses {
            match tokio::time::timeout(TIMEOUT, exchange(address, &request)).await {
                Ok(Ok(reply)) => return checked(&reply),
                Ok(Err(error)) => failures.push(format!("{address}: {error:#}")),
                Err(_) => failures.push(format!("{address}: no reply within {TIMEOUT:?}")),
            }
        }
        bail!("slurmctld did not answer: {}", failures.join("; "))
    }
}

/// Frames a request: its length, a header, a munge credential and the body. A slurmdbd connection
/// puts the message type before the header too.
async fn frame(
    munge: &Path,
    decoder: Option<u32>,
    kind: u16,
    flags: u16,
    body: &[u8],
) -> Result<Vec<u8>> {
    let mut hash = vec![HASH_NONE];
    hash.extend(kind.to_be_bytes());
    let credential = munge::credential(munge, decoder, &hash).await?;
    let mut header = Pack::default();
    if flags & SLURMDBD_CONNECTION != 0 {
        header.u16(kind);
    }
    header
        .u16(PROTOCOL)
        .u16(flags)
        .u16(kind)
        .u32(u32::try_from(body.len())?)
        .u16(0)
        .u16(0)
        .u16(0);
    let mut auth = Pack::default();
    auth.u32(AUTH_MUNGE).str(Some(&credential));
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

/// Strips the header and credential, and turns a nonzero return code into an error.
fn checked(frame: &[u8]) -> Result<Reply> {
    let mut unpack = Unpack::new(frame);
    let version = unpack.u16()?;
    if version != PROTOCOL {
        bail!("slurmctld replied in protocol {version:#x}, not {PROTOCOL:#x}");
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
    Ok(Reply { kind, body })
}

/// A nonzero code from a `RESPONSE_SLURM_RC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReturnCode(pub u32);

/// The code for a job the controller does not know.
pub const INVALID_JOB_ID: ReturnCode = ReturnCode(2017);

impl std::fmt::Display for ReturnCode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Slurm error {}", self.0)
    }
}

impl std::error::Error for ReturnCode {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slurm::scontrol::CpuMap;
    use crate::slurm::source::Source;
    use crate::slurm::{sacct, scontrol, sdiag, sinfo, squeue};
    use glurmo_model::{JobId, UnixTime};

    fn zlurm() -> Controller {
        Controller {
            addresses: vec!["10.7.17.211:6817".into()],
            munge: munge::SOCKET.into(),
            decoder: Some(64030),
        }
    }

    async fn cli<T>(argv: Vec<String>, parse: impl FnOnce(&str) -> anyhow::Result<T>) -> T {
        let exchange = Source::Live.run(argv).await.unwrap();
        parse(&exchange.stdout).unwrap()
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn the_config_matches_scontrol() {
        let mut body = Pack::default();
        body.time(0);
        let reply = zlurm().call(config::REQUEST, &body.0).await.unwrap();
        assert_eq!(reply.kind, config::RESPONSE);
        let config = config::decode(&reply.body).unwrap();
        let site = cli(scontrol::config_argv(), scontrol::parse_config).await;
        assert_eq!(config.site, site);
        assert_eq!(config.slurm_uid, 64030);
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn nodes_and_partitions_match_sinfo_and_scontrol() {
        let controller = zlurm();
        let partitions = controller
            .call(nodes::PARTITION_REQUEST, &nodes::partition_request())
            .await
            .unwrap();
        let partitions = nodes::partitions(&partitions.body).unwrap();
        let reply = controller
            .call(nodes::NODE_REQUEST, &nodes::node_request())
            .await
            .unwrap();
        let wire = nodes::nodes(&reply.body, &partitions).unwrap();
        let listed = cli(sinfo::argv(), sinfo::parse).await;
        let expected = cli(scontrol::partitions_argv(), scontrol::parse_partitions).await;
        let decoded: Vec<_> = partitions
            .into_iter()
            .map(|record| record.partition)
            .collect();
        assert_eq!(decoded, expected);
        assert_eq!(wire.len(), listed.len());
        for (wire, listed) in wire.iter().zip(&listed) {
            let mut listed = listed.clone();
            let mut wire = wire.clone();
            for node in [&mut listed, &mut wire] {
                node.load = None;
                node.mem_free_mb = None;
            }
            assert_eq!(wire, listed);
        }
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn jobs_and_placements_match_squeue_and_scontrol() {
        let controller = zlurm();
        let started = std::time::Instant::now();
        let reply = controller
            .call(jobs::REQUEST, &jobs::request())
            .await
            .unwrap();
        eprintln!(
            "job info: {} bytes in {:?}",
            reply.body.len(),
            started.elapsed()
        );
        let decoded = jobs::decode(&reply.body).unwrap();
        let now = UnixTime::now();
        let mut uids: Vec<u32> = decoded.iter().map(jobs::Job::user_id).collect();
        uids.sort_unstable();
        uids.dedup();
        let logins = crate::passwd::logins(&uids).await.unwrap();
        let wire: Vec<_> = decoded
            .iter()
            .map(|job| job.attempt(now, &logins).unwrap())
            .collect();
        let placed: Vec<_> = decoded.iter().map(|job| job.placed().unwrap()).collect();
        let listed = cli(squeue::argv(), squeue::parse).await;
        let nodes = cli(sinfo::argv(), sinfo::parse).await;
        let map = CpuMap::new(&nodes, crate::slurm::Version(23, 11, 4));
        let shown = cli(scontrol::jobs_argv(), |out| scontrol::parse_jobs(out, &map)).await;
        let by_key: std::collections::HashMap<_, _> = listed
            .iter()
            .map(|job| ((job.key, job.array.as_ref().map(|a| a.spec.clone())), job))
            .collect();
        let mut differ = 0;
        for job in &wire {
            match by_key.get(&(job.key, job.array.as_ref().map(|a| a.spec.clone()))) {
                Some(listed) if *listed == job => {}
                Some(listed) => {
                    differ += 1;
                    if differ <= 5 {
                        eprintln!("wire  {job:?}\nsqueue {listed:?}\n");
                    }
                }
                None => differ += 1,
            }
        }
        eprintln!(
            "{} wire jobs, {} squeue jobs, {differ} differ",
            wire.len(),
            listed.len()
        );
        let shown: std::collections::HashMap<_, _> = shown.iter().map(|p| (p.job, p)).collect();
        let mut placements_differ = 0;
        for p in &placed {
            if let Some(s) = shown.get(&p.job)
                && **s != *p
            {
                placements_differ += 1;
                if placements_differ <= 3 {
                    eprintln!("wire     {p:?}\nscontrol {s:?}\n");
                }
            }
        }
        eprintln!("{placements_differ} placements differ of {}", shown.len());
        assert!(
            differ * 100 <= wire.len(),
            "{differ} of {} differ",
            wire.len()
        );
        assert!(placements_differ * 100 <= placed.len());
    }

    #[tokio::test]
    #[ignore = "needs a live slurmctld"]
    async fn stats_match_sdiag() {
        let reply = zlurm()
            .call(stats::REQUEST, &stats::request())
            .await
            .unwrap();
        let wire = stats::decode(&reply.body).unwrap();
        let printed = cli(sdiag::argv(), sdiag::parse).await;
        assert_eq!(wire.jobs_submitted, printed.jobs_submitted);
        assert_eq!(
            wire.main_cycle_mean_us.is_some(),
            printed.main_cycle_mean_us.is_some()
        );
        assert_eq!(wire.backfilled_jobs, printed.backfilled_jobs);
        let kinds = |stats: &glurmo_model::Stats| {
            let mut kinds: Vec<String> = stats.rpcs.iter().map(|rpc| rpc.kind.clone()).collect();
            kinds.sort();
            kinds
        };
        assert_eq!(kinds(&wire), kinds(&printed));
        eprintln!("{:?}\n{:?}", &wire.rpcs[..3], &printed.rpcs[..3]);
    }

    #[tokio::test]
    #[ignore = "needs a live slurmdbd"]
    async fn accounting_matches_sacct() {
        let addresses = vec!["10.7.17.211:6819".to_owned()];
        let socket = std::path::Path::new(munge::SOCKET);
        let now = UnixTime::now();
        let (from, to) = (UnixTime(now.0 - 3600), UnixTime(now.0 - 600));
        let started = std::time::Instant::now();
        let window = accounting::Query::Window(from, to);
        let (records, tres) =
            accounting::records(&addresses, socket, Some(64030), "zlurm", &window, now)
                .await
                .unwrap();
        eprintln!("{} records in {:?}", records.len(), started.elapsed());
        let mut uids: Vec<u32> = records.iter().map(accounting::Record::user_id).collect();
        uids.sort_unstable();
        uids.dedup();
        let logins = crate::passwd::logins(&uids).await.unwrap();
        let mut wire: Vec<_> = records
            .iter()
            .map(|r| r.attempt(tres, &logins).unwrap())
            .collect();
        let mut printed = cli(sacct::history_argv(from, to).unwrap(), sacct::parse_history).await;
        let order = |a: &glurmo_model::Attempt| (a.key, a.array.as_ref().map(|t| t.spec.clone()));
        wire.sort_by_key(order);
        printed.sort_by_key(order);
        assert_eq!(wire.len(), printed.len());
        let differ: Vec<_> = wire.iter().zip(&printed).filter(|(w, p)| w != p).collect();
        for (w, p) in differ.iter().take(3) {
            eprintln!("wire  {w:?}\nsacct {p:?}\n");
        }
        assert!(
            differ.is_empty(),
            "{} of {} differ",
            differ.len(),
            wire.len()
        );

        let ids: Vec<JobId> = printed.iter().take(20).map(|a| a.key.job).collect();
        let jobs = accounting::Query::Jobs(&ids);
        let (records, _) =
            accounting::records(&addresses, socket, Some(64030), "zlurm", &jobs, now)
                .await
                .unwrap();
        let mut wire: Vec<_> = records.iter().map(|r| r.outcome().unwrap()).collect();
        let mut printed = cli(sacct::argv(&ids), sacct::parse).await;
        wire.sort_by_key(|o| o.key);
        printed.sort_by_key(|o| o.key);
        assert_eq!(wire, printed);
    }
}
