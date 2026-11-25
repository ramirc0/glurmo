//! The terminal view. It follows a server's stream, or samples the cluster in-process, and draws a
//! layout of panels.

mod app;
mod die;
mod feed;
mod groups;
mod header;
mod history;
mod jobs;
mod mine;
mod nodes;
mod palette;
mod partitions;
mod queue;
mod sse;
mod ticker;
mod users;
mod view;

use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use jiff::tz::TimeZone;
use ratatui::DefaultTerminal;
use tokio::signal::unix::SignalKind;
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::library::Library;
use app::App;
use feed::{Feed, Update};
use glurmo_model::Login;

/// How often theme and layout files are checked for changes.
const LIBRARY_EVERY: Duration = Duration::from_secs(2);

/// Runs the view until the user quits or a signal asks it to stop. It follows the server at `server`, or samples the cluster
/// in-process when that is `None`. It shows `me`'s jobs as the viewer's, or `$USER`'s when that is
/// `None`. Stderr goes to `$XDG_STATE_HOME/glurmo/tui.log` meanwhile.
///
/// # Errors
///
/// Fails when the sampler cannot start, the terminal cannot be set up, or the log cannot be opened.
pub async fn run(config: &Config, server: Option<&str>, me: Option<Login>) -> Result<()> {
    let stderr = redirect_stderr()?;
    let result = view(config, server, me).await;
    rustix::stdio::dup2_stderr(&stderr)?;
    result
}

/// Starts the feed and shows it, while stderr goes to the log.
async fn view(config: &Config, server: Option<&str>, me: Option<Login>) -> Result<()> {
    let feed = match server {
        Some(server) => Feed::remote(server)?,
        None => Feed::embedded(config).await?,
    };
    let dir = crate::xdg::config_home()?.join("glurmo");
    let me = me.unwrap_or_else(|| {
        Login(
            std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .unwrap_or_default(),
        )
    });
    let mut app = App::new(Library::load(&dir), &config.tui, me, TimeZone::system());
    let (sender, mut updates) = mpsc::channel(64);
    feed.follow(sender.clone());
    let (span, spans) = watch::channel(app.history_span_s());
    feed.watch_usage(spans, sender);

    let stop = crate::shutdown::signals(&[
        SignalKind::interrupt(),
        SignalKind::terminate(),
        SignalKind::hangup(),
    ])?;
    let result = match ratatui::try_init() {
        Ok(mut terminal) => show(&mut terminal, &mut app, &mut updates, &span, &dir, stop).await,
        Err(error) => Err(error.into()),
    };
    ratatui::restore();
    result
}

/// Points stderr at the log, so messages do not land on the screen. Returns the old stderr.
fn redirect_stderr() -> Result<std::os::fd::OwnedFd> {
    let path = crate::xdg::state_home()?.join("glurmo/tui.log");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let stderr = rustix::io::dup(std::io::stderr())?;
    rustix::stdio::dup2_stderr(&log)?;
    Ok(stderr)
}

/// Draws, then waits for a key, an update or a library check, until the user quits or `stop` resolves.
async fn show(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    updates: &mut mpsc::Receiver<Update>,
    span: &watch::Sender<i64>,
    dir: &Path,
    stop: impl Future<Output = SignalKind>,
) -> Result<()> {
    let mut keys = EventStream::new();
    tokio::pin!(stop);
    let mut checks = tokio::time::interval(LIBRARY_EVERY);
    let mut stamp = Library::stamp(dir);
    loop {
        let mut limits = Vec::new();
        terminal.draw(|frame| limits = view::draw(frame, app))?;
        app.limits = limits;
        tokio::select! {
            event = keys.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => app.key(key),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            },
            Some(update) = updates.recv() => app.update(update),
            _ = &mut stop => return Ok(()),
            _ = checks.tick() => {
                let now = Library::stamp(dir);
                if now != stamp {
                    stamp = now;
                    app.library = Library::load(dir);
                }
            }
        }
        if app.quit {
            return Ok(());
        }
        let span_s = app.history_span_s();
        span.send_if_modified(|current| std::mem::replace(current, span_s) != span_s);
    }
}
