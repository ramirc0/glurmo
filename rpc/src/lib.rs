//! Slurm's RPC protocol, spoken directly to slurmctld and slurmdbd.

pub mod accounting;
pub mod config;
pub mod jobs;
mod munge;
pub mod nodes;
mod pack;
mod reasons;
mod rpcs;
mod sack;
pub mod stats;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub use pack::{INFINITE, NO_VAL};
use pack::{Pack, Unpack};

/// A version of Slurm's protocol that glurmo decodes. A controller answers in the client's version
/// and, per Slurm's source, accepts clients up to three releases older than itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    /// Slurm 23.11's, which controllers from 23.11 through 25.05 accept.
    V23_11,
    /// Slurm 25.11's, which controllers from 25.11 through 27.05 accept.
    V25_11,
}

impl Protocol {
    /// Every version glurmo speaks, newest first, as it tries them.
    const NEWEST_FIRST: [Self; 2] = [Self::V25_11, Self::V23_11];

    /// The version as Slurm numbers it.
    fn number(self) -> u16 {
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
struct Session {
    /// Where credentials come from.
    issuer: Issuer,
    /// The UID that may decode our credentials to slurmctld: `SlurmUser`, once known.
    decoder: Option<u32>,
    /// The protocol version.
    protocol: Protocol,
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
struct Controller {
    /// `host:port` addresses to try in order: the primary, then backups. One in standby passes the call
    /// on, as it does for libslurm.
    addresses: Vec<String>,
    /// How requests are signed, and in which protocol.
    session: Session,
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
    async fn call(&self, request: u16, body: &[u8], response: u16) -> Result<Vec<u8>> {
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
struct ReturnCode(u32);

/// The code for a job the controller does not know.
const INVALID_JOB_ID: ReturnCode = ReturnCode(2017);

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn controllers_come_from_slurmctld_host_lines() {
        let conf = "# comment\nSlurmctldHost=ctl-1(192.0.2.11)\nslurmctldhost=backup\nSlurmctldPort=6820-6821\n";
        assert_eq!(controllers_in(conf), ["192.0.2.11:6820", "backup:6820"]);
        assert_eq!(controllers_in("ControlMachine=ctl\n"), ["ctl:6817"]);
        assert_eq!(controllers_in("ClusterName=x\n"), Vec::<String>::new());
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
}
