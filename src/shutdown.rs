//! The signals that ask glurmo to stop, so it can return and let destructors clean up.

use anyhow::Result;
use tokio::signal::unix::{SignalKind, signal};

/// Listens for SIGINT, SIGTERM and SIGHUP from now on. The future resolves on the first of them.
///
/// # Errors
///
/// Fails when a handler cannot be installed.
pub fn signals() -> Result<impl Future<Output = ()>> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sigterm_ends_the_wait() {
        let stop = signals().unwrap();
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
