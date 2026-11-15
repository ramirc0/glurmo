//! Shares the sampler's deltas with every client: the cluster as of the latest delta, the last hour of
//! deltas for clients that reconnect, and a broadcast of new ones.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::model::{Cluster, Delta, Event, UnixTime};
use crate::sampler::Health;
use crate::slurm::Site;

/// How long a delta stays available to clients that reconnect.
const RING_S: i64 = 3_600;

/// Deltas a client can fall behind before it gets a fresh snapshot instead.
const LAG: usize = 64;

/// Names one delta across server restarts: the boot it came from and its sequence number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventId {
    /// When the server started.
    pub boot: UnixTime,
    /// The delta's sequence number, or the last one a snapshot includes.
    pub seq: u64,
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}-{}", self.boot.0, self.seq)
    }
}

impl FromStr for EventId {
    type Err = anyhow::Error;

    /// Parses `<boot>-<seq>`.
    fn from_str(text: &str) -> Result<Self> {
        let (boot, seq) = text.split_once('-').context("no dash")?;
        Ok(Self {
            boot: UnixTime(boot.parse()?),
            seq: seq.parse()?,
        })
    }
}

/// The whole cluster at one sequence number, for a client that starts from scratch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The last delta it includes.
    pub seq: u64,
    /// Site facts, such as what the scheduler hides.
    pub site: Site,
    /// The cluster.
    #[serde(flatten)]
    pub cluster: Cluster,
}

/// What a client receives next.
#[derive(Clone, Debug)]
pub enum Message {
    /// The whole cluster. Later deltas apply on top of it.
    Snapshot(Box<Snapshot>),
    /// One tick's changes.
    Delta(Arc<Delta>),
}

impl Message {
    /// The sequence number a client should resume after.
    pub fn seq(&self) -> u64 {
        match self {
            Self::Snapshot(snapshot) => snapshot.seq,
            Self::Delta(delta) => delta.seq,
        }
    }
}

/// A handle on the shared state. Clones share it.
#[derive(Clone)]
pub struct Hub(Arc<Shared>);

struct Shared {
    boot: UnixTime,
    site: Site,
    state: Mutex<State>,
    sender: broadcast::Sender<Arc<Delta>>,
}

#[derive(Default)]
struct State {
    seq: u64,
    cluster: Cluster,
    ring: VecDeque<Arc<Delta>>,
    health: Health,
}

impl Hub {
    /// A hub for a cluster with these site facts, booted now.
    pub fn new(site: Site) -> Self {
        Self::booted(site, UnixTime::now())
    }

    fn booted(site: Site, boot: UnixTime) -> Self {
        let state = State {
            cluster: Cluster {
                hides_jobs: site.hides_jobs(),
                ..Cluster::default()
            },
            ..State::default()
        };
        Self(Arc::new(Shared {
            boot,
            site,
            state: Mutex::new(state),
            sender: broadcast::channel(LAG).0,
        }))
    }

    /// When this hub started. It tells a client whether its last event ID still means anything.
    pub fn boot(&self) -> UnixTime {
        self.0.boot
    }

    /// Numbers a tick's events, applies them to the cluster, keeps them for an hour and sends them to
    /// every subscriber. A tick that changed nothing sends nothing.
    pub fn publish(&self, at: UnixTime, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        let mut state = self.lock();
        state.seq += 1;
        let delta = Arc::new(Delta {
            seq: state.seq,
            at,
            events,
        });
        state.cluster.apply(&delta);
        state.ring.push_back(Arc::clone(&delta));
        while state
            .ring
            .front()
            .is_some_and(|oldest| oldest.at.0 < at.0 - RING_S)
        {
            state.ring.pop_front();
        }
        // Sent under the lock, so a client that subscribes and then locks sees each delta once at least.
        // Nobody listening is not an error.
        let _ = self.0.sender.send(delta);
    }

    /// A stream of every delta published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Delta>> {
        self.0.sender.subscribe()
    }

    /// The whole cluster as of the latest delta.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_of(&self.lock())
    }

    fn snapshot_of(&self, state: &State) -> Snapshot {
        Snapshot {
            seq: state.seq,
            site: self.0.site.clone(),
            cluster: state.cluster.clone(),
        }
    }

    /// Follows the deltas for a client that last saw `last`. When `last` is from this boot and the ring
    /// still holds every delta after it, the client gets only those. Otherwise it starts with a snapshot.
    pub fn follow(&self, last: Option<EventId>) -> Follow {
        let receiver = self.subscribe();
        let state = self.lock();
        let resumable = last.filter(|last| {
            let next_kept = state
                .ring
                .front()
                .map_or(state.seq + 1, |oldest| oldest.seq);
            last.boot == self.0.boot && last.seq <= state.seq && last.seq + 1 >= next_kept
        });
        let backlog: VecDeque<Message> = match resumable {
            Some(last) => state
                .ring
                .iter()
                .filter(|delta| delta.seq > last.seq)
                .map(|delta| Message::Delta(Arc::clone(delta)))
                .collect(),
            None => VecDeque::from([Message::Snapshot(Box::new(self.snapshot_of(&state)))]),
        };
        Follow {
            hub: self.clone(),
            receiver,
            backlog,
            seq: state.seq,
        }
    }

    /// Every delta in the ring, oldest first.
    #[cfg(test)]
    pub fn deltas(&self) -> Vec<Arc<Delta>> {
        self.lock().ring.iter().cloned().collect()
    }

    /// Reads the cluster as of the latest delta.
    pub fn read<T>(&self, read: impl FnOnce(&Cluster) -> T) -> T {
        read(&self.lock().cluster)
    }

    /// Replaces the sampler's per-command health.
    pub fn report(&self, health: Health) {
        self.lock().health = health;
    }

    /// The sampler's per-command health.
    pub fn health(&self) -> Health {
        self.lock().health.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every update leaves the state whole, so a panic elsewhere cannot corrupt it.
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One client's place in the stream.
pub struct Follow {
    hub: Hub,
    receiver: broadcast::Receiver<Arc<Delta>>,
    backlog: VecDeque<Message>,
    /// The last sequence number the client has, or will have once the backlog is out.
    seq: u64,
}

impl Follow {
    /// The next message, waiting for one if needed. A client that fell more than 64 deltas behind gets a
    /// fresh snapshot. Returns `None` once the hub is gone.
    pub async fn next(&mut self) -> Option<Message> {
        if let Some(message) = self.backlog.pop_front() {
            return Some(message);
        }
        loop {
            match self.receiver.recv().await {
                Ok(delta) if delta.seq <= self.seq => {}
                Ok(delta) => {
                    self.seq = delta.seq;
                    return Some(Message::Delta(delta));
                }
                Err(RecvError::Lagged(_)) => {
                    let snapshot = self.hub.snapshot();
                    self.seq = snapshot.seq;
                    return Some(Message::Snapshot(Box::new(snapshot)));
                }
                Err(RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Stats;

    const BOOT: UnixTime = UnixTime(1_000);

    fn hub() -> Hub {
        Hub::booted(Site::zlurm(), BOOT)
    }

    /// Publishes ticks 10 s apart from `BOOT`, numbered `from` to `to`.
    fn publish(hub: &Hub, ticks: std::ops::RangeInclusive<i64>) {
        for tick in ticks {
            hub.publish(
                UnixTime(BOOT.0 + 10 * tick),
                vec![Event::Stats(Stats::default())],
            );
        }
    }

    fn id(boot: UnixTime, seq: u64) -> EventId {
        EventId { boot, seq }
    }

    /// What `follow` sends next, without waiting: `Ok(seq)` for a delta, `Err(seq)` for a snapshot.
    fn next(follow: &mut Follow) -> Option<Result<u64, u64>> {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(message) = std::pin::pin!(follow.next()).poll(&mut context)
        else {
            return None;
        };
        Some(match message? {
            Message::Delta(delta) => Ok(delta.seq),
            Message::Snapshot(snapshot) => Err(snapshot.seq),
        })
    }

    fn drain(follow: &mut Follow) -> Vec<Result<u64, u64>> {
        std::iter::from_fn(|| next(follow)).collect()
    }

    #[test]
    fn a_new_client_starts_with_a_snapshot_then_gets_each_delta() {
        let hub = hub();
        publish(&hub, 1..=3);
        let mut follow = hub.follow(None);
        assert_eq!(drain(&mut follow), [Err(3)]);
        publish(&hub, 4..=5);
        assert_eq!(drain(&mut follow), [Ok(4), Ok(5)]);
    }

    #[test]
    fn a_client_that_resumes_inside_the_ring_gets_only_what_it_missed() {
        let hub = hub();
        publish(&hub, 1..=5);
        assert_eq!(
            drain(&mut hub.follow(Some(id(BOOT, 2)))),
            [Ok(3), Ok(4), Ok(5)]
        );
        assert_eq!(drain(&mut hub.follow(Some(id(BOOT, 5)))), []);
    }

    #[test]
    fn an_id_from_another_boot_or_the_future_gets_a_snapshot() {
        let hub = hub();
        publish(&hub, 1..=5);
        assert_eq!(drain(&mut hub.follow(Some(id(UnixTime(999), 2)))), [Err(5)]);
        assert_eq!(drain(&mut hub.follow(Some(id(BOOT, 9)))), [Err(5)]);
    }

    #[test]
    fn deltas_older_than_an_hour_leave_the_ring() {
        let hub = hub();
        publish(&hub, 1..=400);
        let oldest = hub.lock().ring.front().unwrap().seq;
        assert_eq!(
            oldest, 40,
            "tick 400 is at 4,000 s, so the ring starts at 400 s"
        );
        assert_eq!(drain(&mut hub.follow(Some(id(BOOT, 39)))).len(), 361);
        assert_eq!(drain(&mut hub.follow(Some(id(BOOT, 38)))), [Err(400)]);
    }

    #[test]
    fn a_lagging_client_gets_a_fresh_snapshot_and_carries_on() {
        let hub = hub();
        let mut follow = hub.follow(None);
        assert_eq!(next(&mut follow), Some(Err(0)));
        publish(&hub, 1..=100);
        assert_eq!(next(&mut follow), Some(Err(100)));
        assert_eq!(
            next(&mut follow),
            None,
            "the deltas the snapshot covers are skipped"
        );
        publish(&hub, 101..=101);
        assert_eq!(next(&mut follow), Some(Ok(101)));
    }

    #[test]
    fn event_ids_round_trip() {
        let id = EventId {
            boot: BOOT,
            seq: 42,
        };
        assert_eq!(id.to_string(), "1000-42");
        assert_eq!("1000-42".parse::<EventId>().unwrap(), id);
        assert!("1000".parse::<EventId>().is_err());
        assert!("x-1".parse::<EventId>().is_err());
    }

    #[tokio::test]
    async fn snapshots_and_deltas_read_back_as_they_were_written() {
        let hub = crate::fixture::busy_stored_hub().await;
        let snapshot = hub.snapshot();
        assert!(!snapshot.cluster.running.is_empty());
        assert!(!snapshot.cluster.queue.is_empty());
        assert!(!snapshot.cluster.users.is_empty());
        let json = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&json).unwrap(), snapshot);
        for delta in hub.deltas() {
            let json = serde_json::to_string(&*delta).unwrap();
            assert_eq!(serde_json::from_str::<Delta>(&json).unwrap(), *delta);
        }
    }

    #[test]
    fn snapshots_serialize_running_and_queue_as_lists() {
        let json = serde_json::to_value(hub().snapshot()).unwrap();
        assert_eq!(json["seq"], 0);
        assert_eq!(json["site"]["cluster"], "zlurm");
        assert!(json["running"].is_array());
        assert!(json["queue"].is_array());
        assert!(json["users"].is_object());
    }
}
