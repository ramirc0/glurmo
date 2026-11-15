//! The signals that ask glurmo to stop, so it can return and let destructors clean up.

use std::task::Poll;

use anyhow::Result;
use tokio::signal::unix::{SignalKind, signal};

/// Listens for `kinds` of signal from now on. The future resolves on the first that arrives.
///
/// # Errors
///
/// Fails when a handler cannot be installed.
pub fn signals(kinds: &[SignalKind]) -> Result<impl Future<Output = ()> + use<>> {
    let mut streams = kinds
        .iter()
        .map(|&kind| signal(kind))
        .collect::<std::io::Result<Vec<_>>>()?;
    Ok(std::future::poll_fn(move |cx| {
        if streams
            .iter_mut()
            .any(|stream| stream.poll_recv(cx).is_ready())
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sigterm_ends_the_wait() {
        let stop = signals(&[SignalKind::terminate()]).unwrap();
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        tokio::time::timeout(std::time::Duration::from_secs(5), stop)
            .await
            .unwrap();
    }
}
