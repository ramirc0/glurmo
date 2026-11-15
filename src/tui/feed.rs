//! Where the terminal view gets the cluster from: a server's stream, or a sampler in this process.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};

use super::sse::SseReader;
use crate::config::Config;
use crate::hub::{Hub, Message};
use crate::sampler::Sampler;
use crate::slurm::Slurm;
use crate::slurm::source::Source;
use crate::store::{By, History, Settings, Store, UsageQuery};
use rtviz_model::UnixTime;
use rtviz_model::usage::Usage;

/// How long to wait before reconnecting to a server, as the server's `retry` field also says.
const RETRY: Duration = Duration::from_secs(5);

/// A stream that sends nothing for this long is dead. The server sends a keepalive every 15 s.
const SILENCE: Duration = Duration::from_secs(45);

/// How often usage history is read again.
const USAGE_EVERY: Duration = Duration::from_secs(60);

/// Something that changes what the view shows.
#[derive(Debug)]
pub enum Update {
    /// A snapshot or a delta.
    Message(Message),
    /// The connection to the server came up or went down.
    Link(Link),
    /// Usage history, or why it could not be read.
    Usage(Result<Usage, String>),
}

/// The state of the connection to the server. The embedded sampler is always [`Link::Live`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// Not connected yet.
    Connecting,
    /// Following the stream.
    Live,
    /// The stream broke, and reconnecting has failed since `since`.
    Lost {
        /// When the stream broke.
        since: UnixTime,
        /// Why the last attempt failed.
        error: String,
    },
}

/// The source of snapshots, deltas and usage.
#[derive(Clone)]
pub enum Feed {
    /// A server's stream and usage API.
    Remote {
        /// The server's URL, without a trailing slash.
        base: String,
        /// The HTTP client.
        client: reqwest::Client,
    },
    /// A sampler in this process, with history in a temporary store.
    Embedded {
        /// The sampler's hub.
        hub: Hub,
        /// The store's history.
        history: Arc<History>,
        /// The directory that holds the store. It is deleted with the last clone.
        _dir: Arc<tempfile::TempDir>,
    },
}

impl Feed {
    /// Follows the server at `base`, such as `http://js029:8765`.
    ///
    /// # Errors
    ///
    /// Fails when the HTTP client cannot be built.
    pub fn remote(base: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(RETRY)
            .read_timeout(SILENCE)
            .build()?;
        Ok(Self::Remote {
            base: base.trim_end_matches('/').to_owned(),
            client,
        })
    }

    /// Starts a sampler in this process. Its store lives in a temporary directory under
    /// `$XDG_RUNTIME_DIR`, or the system's temporary directory, and backfills the last day.
    ///
    /// # Errors
    ///
    /// Fails when Slurm cannot be reached or the store cannot be created.
    pub async fn embedded(config: &Config) -> Result<Self> {
        let slurm = Slurm::connect(Source::Live).await?;
        let parent = std::env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let dir = tempfile::Builder::new()
            .prefix("rtviz-")
            .tempdir_in(&parent)
            .with_context(|| format!("cannot create a store under {}", parent.display()))?;
        let path = dir.path().join("store.sqlite");
        let settings = Settings {
            backfill_days: 1,
            keep_days: None,
            backup: dir.path().join("backup.sqlite"),
        };
        let store = Store::open(&path, settings)?;
        let hub = Hub::new(slurm.site().clone());
        let mut sampler = Sampler::new(slurm, config, Some(store), hub.clone());
        tokio::spawn(async move { sampler.run(None).await });
        Ok(Self::Embedded {
            hub,
            history: Arc::new(History::open(&path)),
            _dir: Arc::new(dir),
        })
    }

    /// Sends a snapshot, then each delta, to `updates` until it closes.
    /// A server stream that breaks is resumed after the last event that arrived.
    pub fn follow(&self, updates: mpsc::Sender<Update>) {
        match self {
            Self::Remote { base, client } => {
                tokio::spawn(follow_remote(client.clone(), base.clone(), updates));
            }
            Self::Embedded { hub, .. } => {
                let mut follow = hub.follow(None);
                tokio::spawn(async move {
                    let _ = updates.send(Update::Link(Link::Live)).await;
                    while let Some(message) = follow.next().await {
                        if updates.send(Update::Message(message)).await.is_err() {
                            return;
                        }
                    }
                });
            }
        }
    }

    /// Sends the usage over the last `span` seconds to `updates` every minute, and again whenever
    /// `span` changes, until either closes.
    pub fn watch_usage(&self, mut span: watch::Receiver<i64>, updates: mpsc::Sender<Update>) {
        let feed = self.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(USAGE_EVERY);
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    changed = span.changed() => if changed.is_err() { return },
                }
                let span_s = *span.borrow_and_update();
                let usage = feed
                    .usage(span_s)
                    .await
                    .map_err(|error| format!("{error:#}"));
                if updates.send(Update::Usage(usage)).await.is_err() {
                    return;
                }
            }
        });
    }

    /// Usage by login over the last `span_s` seconds, in steps of five minutes or more.
    async fn usage(&self, span_s: i64) -> Result<Usage> {
        let to = UnixTime::now();
        let from = UnixTime(to.0 - span_s);
        let step_s = step_for(span_s);
        let rows = match self {
            Self::Remote { base, client } => {
                let url = format!(
                    "{base}/api/usage?from={}&to={}&step={step_s}&by=login",
                    from.0, to.0
                );
                let body = client
                    .get(url)
                    .send()
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;
                serde_json::from_slice(&body)?
            }
            Self::Embedded { history, .. } => {
                let history = Arc::clone(history);
                let query = UsageQuery {
                    from,
                    to,
                    step_s,
                    by: By::Login,
                };
                tokio::task::spawn_blocking(move || history.usage(&query, to)).await??
            }
        };
        Ok(Usage {
            from,
            to,
            step_s,
            steps: rows,
        })
    }
}

/// A step that gives at most about 600 steps over `span_s`, in whole five-minute bins.
fn step_for(span_s: i64) -> i64 {
    ((span_s / 600).max(300) + 299) / 300 * 300
}

/// Follows the server's stream until `updates` closes, reconnecting after each break.
async fn follow_remote(client: reqwest::Client, base: String, updates: mpsc::Sender<Update>) {
    let mut last_id: Option<String> = None;
    let mut down_since: Option<UnixTime> = None;
    loop {
        let mut request = client.get(format!("{base}/api/events"));
        if let Some(id) = &last_id {
            request = request.header("last-event-id", id);
        }
        let sent = request.send().await;
        let error = match sent.and_then(reqwest::Response::error_for_status) {
            Ok(response) => {
                down_since = None;
                if updates.send(Update::Link(Link::Live)).await.is_err() {
                    return;
                }
                match read(response, &mut last_id, &updates).await {
                    Ok(()) => "the server ended the stream".to_owned(),
                    Err(error) => format!("{error:#}"),
                }
            }
            Err(error) => format!("{error:#}"),
        };
        if updates.is_closed() {
            return;
        }
        let since = *down_since.get_or_insert_with(UnixTime::now);
        let _ = updates
            .send(Update::Link(Link::Lost { since, error }))
            .await;
        tokio::time::sleep(RETRY).await;
    }
}

/// Reads one response's events into `updates`, keeping the ID of the last one in `last`.
/// Returns when the stream ends or `updates` closes.
async fn read(
    response: reqwest::Response,
    last: &mut Option<String>,
    updates: &mpsc::Sender<Update>,
) -> Result<()> {
    let mut reader = SseReader::default();
    let mut body = response.bytes_stream();
    while let Some(chunk) = body.next().await {
        for event in reader.read(&chunk?) {
            let message = match event.kind.as_str() {
                "snapshot" => Message::Snapshot(Box::new(
                    serde_json::from_str(&event.data).context("a bad snapshot")?,
                )),
                "delta" => Message::Delta(Arc::new(
                    serde_json::from_str(&event.data).context("a bad delta")?,
                )),
                _ => continue,
            };
            if event.id.is_some() {
                *last = event.id;
            }
            if updates.send(Update::Message(message)).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::Library;
    use crate::slurm::Site;
    use rtviz_model::{Event, Stats};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;

    /// Forwards connections on its own port to `target`. Pulling the plug drops every connection open
    /// so far, while new ones still go through.
    struct Proxy {
        port: u16,
        connections: std::sync::Arc<std::sync::Mutex<Vec<JoinHandle<()>>>>,
    }

    impl Proxy {
        async fn start(target: std::net::SocketAddr) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let connections: std::sync::Arc<std::sync::Mutex<Vec<JoinHandle<()>>>> =
                std::sync::Arc::default();
            let open = std::sync::Arc::clone(&connections);
            tokio::spawn(async move {
                loop {
                    let (mut client, _) = listener.accept().await.unwrap();
                    let connection = tokio::spawn(async move {
                        let mut server = TcpStream::connect(target).await.unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    });
                    open.lock().unwrap().push(connection);
                }
            });
            Self { port, connections }
        }

        fn pull_the_plug(&self) {
            for connection in self.connections.lock().unwrap().drain(..) {
                connection.abort();
            }
        }
    }

    async fn next(updates: &mut mpsc::Receiver<Update>) -> Update {
        tokio::time::timeout(Duration::from_secs(15), updates.recv())
            .await
            .expect("an update within 15 s")
            .expect("the feed is still running")
    }

    fn publish(hub: &Hub) {
        hub.publish(UnixTime::now(), vec![Event::Stats(Stats::default())]);
    }

    #[tokio::test]
    async fn a_remote_feed_resumes_after_a_network_pull_without_a_fresh_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtviz.sqlite");
        let settings = Settings {
            backfill_days: 0,
            keep_days: None,
            backup: dir.path().join("backup.sqlite"),
        };
        Store::open(&path, settings).unwrap();
        let hub = Hub::new(Site::zlurm());
        publish(&hub);
        let library = watch::channel(Library::load(dir.path())).1;
        let router = crate::server::router(hub.clone(), History::open(&path), library);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let proxy = Proxy::start(server).await;

        let feed = Feed::remote(&format!("http://127.0.0.1:{}/", proxy.port)).unwrap();
        let (sender, mut updates) = mpsc::channel(16);
        feed.follow(sender);
        assert!(matches!(next(&mut updates).await, Update::Link(Link::Live)));
        let Update::Message(Message::Snapshot(snapshot)) = next(&mut updates).await else {
            panic!("a snapshot first");
        };
        assert_eq!(snapshot.seq, 1);
        publish(&hub);
        assert!(matches!(
            next(&mut updates).await,
            Update::Message(Message::Delta(delta)) if delta.seq == 2
        ));

        proxy.pull_the_plug();
        assert!(matches!(
            next(&mut updates).await,
            Update::Link(Link::Lost { .. })
        ));
        publish(&hub);
        assert!(matches!(next(&mut updates).await, Update::Link(Link::Live)));
        assert!(
            matches!(
                next(&mut updates).await,
                Update::Message(Message::Delta(delta)) if delta.seq == 3
            ),
            "the missed delta, not a snapshot"
        );

        let usage = feed.usage(86_400).await.unwrap();
        assert_eq!((usage.step_s, usage.steps), (300, Vec::new()));
    }

    #[test]
    fn steps_are_whole_bins_and_at_most_about_600() {
        assert_eq!(step_for(3_600), 300);
        assert_eq!(step_for(86_400), 300);
        assert_eq!(step_for(7 * 86_400), 1_200);
        assert_eq!(step_for(30 * 86_400), 4_500);
    }
}
