//! Asks the local munged for a credential, as libmunge's `munge_encode` does.
//!
//! On Linux, munged reads the caller's UID from the socket, so one request and one reply suffice.

use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::pack::{Pack, Unpack};

/// The socket munged listens on unless `AuthInfo` names another.
pub const SOCKET: &str = "/run/munge/munge.socket.2";

const MAGIC: u32 = 0x0060_6D4B;
const VERSION: u8 = 4;
const ENCODE_REQUEST: u8 = 2;
const ENCODE_RESPONSE: u8 = 3;
const DEFAULT: u8 = 1;
const ANY: u32 = u32::MAX;
const HEADER_LEN: usize = 11;

/// Encodes `payload` into a credential that only `decoder` may decode, or anyone when it is `None`.
///
/// # Errors
///
/// Fails when munged is unreachable or refuses.
pub async fn credential(socket: &Path, decoder: Option<u32>, payload: &[u8]) -> Result<String> {
    let mut body = Pack::default();
    body.0.extend([DEFAULT, DEFAULT, DEFAULT, 0]);
    body.u32(0)
        .u32(decoder.unwrap_or(ANY))
        .u32(ANY)
        .mem(payload);
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("cannot reach munged at {}", socket.display()))?;
    stream.write_all(&header(ENCODE_REQUEST, &body.0)).await?;
    stream.write_all(&body.0).await?;

    let mut head = [0; HEADER_LEN];
    stream.read_exact(&mut head).await?;
    let mut fields = Unpack::new(&head);
    let (magic, version, kind) = (fields.u32()?, fields.u8()?, fields.u8()?);
    fields.u8()?;
    let len = fields.u32()? as usize;
    if (magic, version, kind) != (MAGIC, VERSION, ENCODE_RESPONSE) {
        bail!("munged sent message {kind} version {version} with magic {magic:#x}");
    }
    let mut reply = vec![0; len];
    stream.read_exact(&mut reply).await?;
    let mut reply = Unpack::new(&reply);
    let error = reply.u8()?;
    let message_len = reply.u8()?;
    let message = reply.take(usize::from(message_len))?;
    if error != 0 {
        let message = String::from_utf8_lossy(message);
        bail!("munged refused: {}", message.trim_end_matches('\0'));
    }
    let credential = reply.mem()?.context("munged sent an empty credential")?;
    let credential = credential.strip_suffix(b"\0").unwrap_or(credential);
    Ok(String::from_utf8(credential.to_vec())?)
}

fn header(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut pack = Pack::default();
    pack.u32(MAGIC);
    pack.0.extend([VERSION, kind, 0]);
    pack.u32(u32::try_from(body.len()).expect("a small request"));
    pack.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn munged_encodes_a_credential() {
        let socket = Path::new(SOCKET);
        if !socket.exists() {
            eprintln!("no munged here");
            return;
        }
        let credential = credential(socket, None, b"glurmo").await.unwrap();
        assert!(credential.starts_with("MUNGE:"), "{credential}");
        assert!(credential.ends_with(':'), "{credential}");
    }
}
