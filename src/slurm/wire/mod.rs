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
use super::recording::{Answer, Call};
use super::scontrol::Placed;
use glurmo_model::{Attempt, Login, UnixTime};
use pack::{Pack, Unpack};

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
    /// `host:port` addresses to try in order: the primary, then backups.
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
                Ok(Ok(reply)) => {
                    self.answered.store(i, Ordering::Relaxed);
                    return checked(&reply, response, self.session.protocol);
                }
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

impl std::fmt::Display for ReturnCode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Slurm error {}", self.0)
    }
}

impl std::error::Error for ReturnCode {}

fn placed(jobs: &[jobs::Job]) -> Result<Vec<Placed>> {
    jobs.iter().map(jobs::Job::placed).collect()
}

/// The accounting records that convert. One that does not is logged and skipped, so it cannot block
/// the rest.
fn readable<T>(converted: impl Iterator<Item = Result<T>>) -> Vec<T> {
    converted
        .filter_map(|record| {
            record
                .inspect_err(|error| eprintln!("skipped an accounting record: {error:#}"))
                .ok()
        })
        .collect()
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

/// Answers calls by speaking Slurm's protocol to slurmctld and slurmdbd.
pub struct Wire {
    controller: Controller,
    cluster: String,
    accounting: Vec<String>,
    logins: HashMap<u32, Login>,
    unknown: HashSet<u32>,
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
        Ok(Self {
            controller: Controller {
                addresses,
                session: Session {
                    issuer: Issuer::find()?,
                    decoder: None,
                    protocol: Protocol::NEWEST_FIRST[0],
                },
                answered: AtomicUsize::new(0),
            },
            cluster: String::new(),
            accounting: Vec::new(),
            logins: HashMap::new(),
            unknown: HashSet::new(),
            next_placements: None,
        })
    }

    /// Reads the configuration in the newest protocol the controller accepts, and keeps that protocol.
    async fn connect(&mut self) -> Result<super::Site> {
        let mut failures = Vec::new();
        for protocol in Protocol::NEWEST_FIRST {
            self.controller.session.protocol = protocol;
            match self.config().await {
                Ok(site) => return Ok(site),
                Err(error) => failures.push(format!("{protocol:?}: {error:#}")),
            }
        }
        bail!("no protocol glurmo speaks worked: {}", failures.join("; "))
    }

    async fn config(&mut self) -> Result<super::Site> {
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
        if let (Issuer::Munge(socket), Some(named)) = (&mut session.issuer, config.munge) {
            *socket = named;
        }
        self.cluster.clone_from(&config.site.cluster);
        self.accounting = config.accounting;
        Ok(config.site)
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
            Call::Config => Answer::Config(self.connect().await?),
            Call::Jobs => Answer::Jobs(self.attempts().await?),
            Call::Nodes => {
                let partitions = self.partitions().await?;
                let reply = self
                    .controller
                    .call(
                        nodes::NODE_REQUEST,
                        &nodes::node_request(),
                        nodes::NODE_RESPONSE,
                    )
                    .await?;
                Answer::Nodes(nodes::nodes(
                    &reply,
                    &partitions,
                    self.controller.session.protocol,
                )?)
            }
            Call::Partitions => Answer::Partitions(
                self.partitions()
                    .await?
                    .into_iter()
                    .map(|record| record.partition)
                    .collect(),
            ),
            Call::Placements => Answer::Placements(match listed {
                Some(placements) => placements?,
                None => placed(&self.jobs(jobs::REQUEST, &jobs::request()).await?)?,
            }),
            Call::Lookup { key, .. } => {
                let found = match self
                    .jobs(
                        jobs::SINGLE_REQUEST,
                        &jobs::single_request(key.job, self.controller.session.protocol),
                    )
                    .await
                {
                    Ok(jobs) => jobs.into_iter().find(|job| job.id() == key.job),
                    Err(error) if error.downcast_ref::<ReturnCode>() == Some(&INVALID_JOB_ID) => {
                        None
                    }
                    Err(error) => return Err(error),
                };
                Answer::Lookup(match found {
                    Some(job) => Lookup::Found(job.placed()?),
                    None => Lookup::Gone,
                })
            }
            Call::Outcomes(ids) => {
                let (records, _) = self.accounting(&accounting::Query::Jobs(ids)).await?;
                Answer::Outcomes(readable(records.iter().map(accounting::Record::outcome)))
            }
            Call::History { from, to } => {
                let window = accounting::Query::Window(*from, *to);
                let (records, tres) = self.accounting(&window).await?;
                self.learn_logins(records.iter().map(accounting::Record::user_id))
                    .await;
                Answer::History(readable(
                    records
                        .iter()
                        .map(|record| record.attempt(tres, &self.logins)),
                ))
            }
            Call::Stats => {
                let reply = self
                    .controller
                    .call(stats::REQUEST, &stats::request(), stats::RESPONSE)
                    .await?;
                Answer::Stats(stats::decode(&reply, self.controller.session.protocol)?)
            }
        })
    }

    /// Every job as squeue lists it. Keeps their placements for the placement call that follows.
    async fn attempts(&mut self) -> Result<Vec<Attempt>> {
        let mut jobs = self.jobs(jobs::REQUEST, &jobs::request()).await?;
        self.next_placements = Some(placed(&jobs));
        jobs::combine_pending_arrays(&mut jobs);
        self.learn_logins(jobs.iter().map(jobs::Job::user_id)).await;
        let now = UnixTime::now();
        jobs.iter()
            .filter(|job| job.submitted())
            .map(|job| job.attempt(now, &self.logins))
            .collect()
    }

    async fn jobs(&self, kind: u16, request: &[u8]) -> Result<Vec<jobs::Job>> {
        let reply = self.controller.call(kind, request, jobs::RESPONSE).await?;
        jobs::decode(&reply, self.controller.session.protocol)
    }

    async fn partitions(&self) -> Result<Vec<nodes::PartitionRecord>> {
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

    async fn accounting(
        &self,
        query: &accounting::Query<'_>,
    ) -> Result<(Vec<accounting::Record>, accounting::Tres)> {
        if self.accounting.is_empty() {
            bail!("the site keeps no accounting in slurmdbd");
        }
        accounting::records(
            &self.accounting,
            &self.controller.session,
            &self.cluster,
            query,
            UnixTime::now(),
        )
        .await
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
            }
            Err(error) => eprintln!("cannot look up logins: {error:#}"),
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

    fn wire_to(addresses: Vec<String>, sack: PathBuf) -> Wire {
        Wire {
            controller: Controller {
                addresses,
                session: Session {
                    issuer: Issuer::Sack(sack),
                    decoder: None,
                    protocol: Protocol::V23_11,
                },
                answered: AtomicUsize::new(0),
            },
            cluster: String::new(),
            accounting: Vec::new(),
            logins: HashMap::new(),
            unknown: HashSet::new(),
            next_placements: None,
        }
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
        let mut wire = wire_to(vec![primary, backup], sack);
        let count = |requests: &std::sync::atomic::AtomicUsize| {
            requests.load(std::sync::atomic::Ordering::SeqCst)
        };
        wire.answer(&Call::Jobs).await.unwrap();
        assert_eq!((count(&refused), count(&answered)), (1, 1));
        wire.answer(&Call::Jobs).await.unwrap();
        wire.answer(&Call::Jobs).await.unwrap();
        assert_eq!((count(&refused), count(&answered)), (1, 3));
    }

    #[tokio::test]
    async fn placements_right_after_jobs_reuse_their_reply() {
        let dir = tempfile::tempdir().unwrap();
        let sack = dir.path().join("sack.socket");
        sack_forever(&sack);
        let mut no_jobs = Pack::default();
        no_jobs.u32(0).time(0).time(0);
        let (address, requests) = controller_answering(jobs::RESPONSE, no_jobs.0).await;
        let mut wire = wire_to(vec![address], sack);
        let count = || requests.load(std::sync::atomic::Ordering::SeqCst);
        wire.answer(&Call::Jobs).await.unwrap();
        wire.answer(&Call::Placements).await.unwrap();
        assert_eq!(count(), 1);
        wire.answer(&Call::Placements).await.unwrap();
        assert_eq!(count(), 2, "a reply serves one placement call only");
        wire.answer(&Call::Jobs).await.unwrap();
        wire.answer(&Call::Partitions).await.ok();
        wire.answer(&Call::Placements).await.unwrap();
        assert_eq!(count(), 5, "another call in between drops the reply");
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
}
