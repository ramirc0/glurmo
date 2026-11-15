//! Slurm's RPC protocol, spoken directly to slurmctld and slurmdbd.

pub mod config;
pub mod jobs;
pub mod munge;
pub mod nodes;
pub mod pack;
pub mod reasons;

use std::path::PathBuf;
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
        let request = self.frame(kind, body).await?;
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

    async fn frame(&self, kind: u16, body: &[u8]) -> Result<Vec<u8>> {
        let mut hash = vec![HASH_NONE];
        hash.extend(kind.to_be_bytes());
        let credential = munge::credential(&self.munge, self.decoder, &hash).await?;
        let mut header = Pack::default();
        header
            .u16(PROTOCOL)
            .u16(0)
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
}

async fn exchange(address: &str, request: &[u8]) -> Result<Vec<u8>> {
    let mut stream = TcpStream::connect(address).await?;
    stream.write_all(request).await?;
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
    use crate::slurm::{scontrol, sinfo, squeue};
    use glurmo_model::UnixTime;

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
}
