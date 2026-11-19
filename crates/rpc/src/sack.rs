//! Asks the local slurmd or sackd for an `auth/slurm` token, as libslurm's `sack_create` does, for sites
//! that sign requests with a shared key instead of munge.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::pack::Pack;

/// The sockets libslurm tries, in order, after `$SLURM_SACK_SOCKET`.
pub const SOCKETS: [&str; 3] = [
    "/run/slurm/sack.socket",
    "/run/slurmctld/sack.socket",
    "/run/slurmdbd/sack.socket",
];

const CREATE: u32 = 64_001;
const ANY: u32 = u32::MAX;

/// The first sack socket that exists: `$SLURM_SACK_SOCKET`, or one of [`SOCKETS`].
pub fn socket() -> Option<PathBuf> {
    std::env::var_os("SLURM_SACK_SOCKET")
        .map(PathBuf::from)
        .into_iter()
        .chain(SOCKETS.iter().map(PathBuf::from))
        .find(|path| path.exists())
}

/// Asks for a token that only `decoder` may verify, or anyone when it is `None`, carrying `payload`.
/// `protocol` is the Slurm protocol version the request speaks.
///
/// # Errors
///
/// Fails when the socket is unreachable or the daemon sends no token.
pub async fn token(
    socket: &Path,
    protocol: u16,
    decoder: Option<u32>,
    payload: &[u8],
) -> Result<String> {
    let mut rest = Pack::default();
    rest.u32(CREATE).u32(decoder.unwrap_or(ANY)).mem(payload);
    let mut request = Pack::default();
    request.u16(protocol).u32(u32::try_from(rest.0.len() + 4)?);
    request.0.extend(rest.0);
    let mut stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("cannot reach sack at {}", socket.display()))?;
    stream.write_all(&request.0).await?;
    let len = stream.read_u32().await? as usize;
    if len == 0 {
        bail!("sack at {} sent no token", socket.display());
    }
    let mut token = vec![0; len];
    stream.read_exact(&mut token).await?;
    let token = token.strip_suffix(b"\0").unwrap_or(&token);
    Ok(String::from_utf8(token.to_vec())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sack_signs_a_token_where_it_runs() {
        let Some(socket) = socket() else {
            eprintln!("no sack here");
            return;
        };
        let token = token(&socket, 44 << 8, None, b"glurmo").await.unwrap();
        assert_eq!(token.split('.').count(), 3, "a JWT: {token}");
    }
}
