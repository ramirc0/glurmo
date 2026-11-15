//! Sums attempts into five-minute usage bins per login and partition.

use std::collections::BTreeMap;

use rtviz_model::{EndState, Login, PartitionName, UnixTime};

/// How long one bin lasts.
pub const BIN_S: i64 = 300;

/// The start of the bin that holds `time`.
pub fn bin_of(time: UnixTime) -> UnixTime {
    UnixTime(time.0.div_euclid(BIN_S) * BIN_S)
}

/// What a rollup needs from one attempt: how long it waited and how long it held cores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The owner.
    pub login: Login,
    /// Where it ran, or the first partition it asked for while pending.
    pub partition: PartitionName,
    /// Array tasks it stands for while pending, 1 otherwise.
    pub tasks: u32,
    /// Cores held while running.
    pub cores: u32,
    /// GPUs held while running.
    pub gpus: u32,
    /// When it was submitted.
    pub submit: UnixTime,
    /// When it took its cores.
    pub start: Option<UnixTime>,
    /// When it freed its cores, or left the queue without running.
    pub end: Option<UnixTime>,
    /// How it ended, once known.
    pub state: Option<EndState>,
}

/// One login's use of one partition during one bin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Core-seconds held.
    pub core_s: u64,
    /// GPU-seconds held.
    pub gpu_s: u64,
    /// Task-seconds spent pending.
    pub wait_s: u64,
    /// Attempts that took their cores.
    pub started: u32,
    /// Attempts that freed their cores.
    pub ended: u32,
    /// Ended attempts whose state counts as a failure.
    pub failed: u32,
}

impl std::ops::AddAssign<&Usage> for Usage {
    fn add_assign(&mut self, other: &Usage) {
        self.core_s += other.core_s;
        self.gpu_s += other.gpu_s;
        self.wait_s += other.wait_s;
        self.started += other.started;
        self.ended += other.ended;
        self.failed += other.failed;
    }
}

/// Sums `spans` over the bin that starts at `bin`. An attempt still pending or running counts up to `now`.
/// Logins and partitions with nothing in the bin are left out.
pub fn rollup(
    bin: UnixTime,
    spans: &[Span],
    now: UnixTime,
) -> BTreeMap<(Login, PartitionName), Usage> {
    let close = UnixTime(bin.0 + BIN_S);
    let inside = |time: Option<UnixTime>| time.is_some_and(|time| bin <= time && time < close);
    let overlap = |from: UnixTime, to: UnixTime| {
        u64::try_from(to.min(close).0 - from.max(bin).0).unwrap_or(0)
    };
    let mut usage: BTreeMap<(Login, PartitionName), Usage> = BTreeMap::new();
    for span in spans {
        let until = span.end.unwrap_or(now);
        let waited = overlap(span.submit, span.start.unwrap_or(until)) * u64::from(span.tasks);
        let held = span.start.map_or(0, |start| overlap(start, until));
        let ended = span.start.is_some() && inside(span.end);
        let row = Usage {
            core_s: held * u64::from(span.cores),
            gpu_s: held * u64::from(span.gpus),
            wait_s: waited,
            started: u32::from(inside(span.start)),
            ended: u32::from(ended),
            failed: u32::from(ended && span.state.is_some_and(EndState::is_failure)),
        };
        if row == Usage::default() {
            continue;
        }
        let total = usage
            .entry((span.login.clone(), span.partition.clone()))
            .or_default();
        *total += &row;
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(login: &str, submit: i64, start: Option<i64>, end: Option<i64>) -> Span {
        Span {
            login: Login(login.into()),
            partition: PartitionName("4hours".into()),
            tasks: 1,
            cores: 4,
            gpus: 1,
            submit: UnixTime(submit),
            start: start.map(UnixTime),
            end: end.map(UnixTime),
            state: Some(EndState::Completed),
        }
    }

    fn key(login: &str) -> (Login, PartitionName) {
        (Login(login.into()), PartitionName("4hours".into()))
    }

    #[test]
    fn bins_start_on_multiples_of_five_minutes() {
        assert_eq!(bin_of(UnixTime(1_794_614_699)), UnixTime(1_794_614_400));
        assert_eq!(bin_of(UnixTime(1_794_614_700)), UnixTime(1_794_614_700));
    }

    #[test]
    fn hand_computed_bin() {
        let mut timeout = span("otter", 0, Some(100), Some(1_250));
        timeout.state = Some(EndState::Timeout);
        let mut range = span("heron", 1_000, None, None);
        range.tasks = 10;
        let mut cancelled = span("heron", 1_100, None, Some(1_150));
        cancelled.state = Some(EndState::Cancelled);
        let spans = [
            timeout,
            span("otter", 1_000, Some(1_060), None),
            range,
            cancelled,
            span("heron", 0, Some(10), Some(20)),
        ];
        let usage = rollup(UnixTime(1_200), &spans, UnixTime(1_400));
        assert_eq!(
            usage[&key("otter")],
            Usage {
                core_s: 4 * 50 + 4 * 200,
                gpu_s: 50 + 200,
                wait_s: 0,
                started: 0,
                ended: 1,
                failed: 1,
            }
        );
        assert_eq!(
            usage[&key("heron")],
            Usage {
                wait_s: 10 * 200,
                ..Usage::default()
            },
            "a running bin counts pending tasks up to now, and a withdrawn attempt is not an end"
        );
        assert_eq!(usage.len(), 2);
    }

    #[test]
    fn core_seconds_add_up_across_bins() {
        let spans = [
            span("otter", 50, Some(130), Some(2_000)),
            span("heron", 0, Some(299), None),
        ];
        let now = UnixTime(1_234);
        let (mut core_s, mut wait_s, mut started, mut ended) = (0, 0, 0, 0);
        for bin in (0..2_400).step_by(300) {
            for usage in rollup(UnixTime(bin), &spans, now).values() {
                core_s += usage.core_s;
                wait_s += usage.wait_s;
                started += usage.started;
                ended += usage.ended;
            }
        }
        assert_eq!(core_s, 4 * (2_000 - 130) + 4 * (1_234 - 299));
        assert_eq!(wait_s, (130 - 50) + 299);
        assert_eq!((started, ended), (2, 1));
    }
}
