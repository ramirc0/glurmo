//! Scheduler and RPC counters, as sdiag reads them.

use super::rpcs::RPCS;

/// Counters since the controller's last daily reset, as sdiag prints them. The scheduler's counters are
/// `None` when the controller leaves them out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Controller threads busy with RPCs.
    pub server_threads: Option<u64>,
    /// Outgoing messages waiting in the agent queue.
    pub agent_queue: Option<u64>,
    /// Accounting messages waiting for slurmdbd.
    pub dbd_queue: Option<u64>,
    /// Jobs submitted.
    pub jobs_submitted: Option<u64>,
    /// Jobs started.
    pub jobs_started: Option<u64>,
    /// Jobs completed.
    pub jobs_completed: Option<u64>,
    /// The main scheduler's mean cycle, in microseconds.
    pub main_cycle_mean_us: Option<u64>,
    /// Jobs the main scheduler looked at in its last cycle.
    pub main_queue_last: Option<u64>,
    /// The backfill scheduler's mean cycle, in microseconds.
    pub backfill_cycle_mean_us: Option<u64>,
    /// Jobs started by backfill since the controller started.
    pub backfilled_jobs: Option<u64>,
    /// RPCs by message type, busiest first.
    pub rpcs: Vec<RpcCount>,
}

/// How often the controller served one message type, and for how long in total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcCount {
    /// The message type as sdiag names it, such as `REQUEST_JOB_INFO`, or its number.
    pub kind: String,
    /// Calls served.
    pub count: u64,
    /// Time spent serving them, in microseconds.
    pub total_us: u64,
}

/// The counters glurmo charts. RPCs come busiest first, as sdiag lists them.
pub fn stats(statistics: &slurm_client::Statistics) -> Stats {
    let (main, backfill) = (&statistics.scheduler, &statistics.backfill);
    let mut rpcs: Vec<RpcCount> = statistics
        .rpcs_by_type
        .iter()
        .map(|rpc| RpcCount {
            kind: rpc_name(rpc.message.0),
            count: rpc.count.into(),
            total_us: microseconds(rpc.time),
        })
        .collect();
    rpcs.sort_by_key(|rpc| std::cmp::Reverse(rpc.count));
    Stats {
        server_threads: Some(statistics.server_threads.into()),
        agent_queue: Some(statistics.agent_queue_size.into()),
        dbd_queue: Some(statistics.dbd_agent_queue_size.into()),
        jobs_submitted: Some(statistics.jobs_submitted.into()),
        jobs_started: Some(statistics.jobs_started.into()),
        jobs_completed: Some(statistics.jobs_completed.into()),
        main_cycle_mean_us: mean(microseconds(main.cycle_sum), main.cycles),
        main_queue_last: Some(main.queue_length.into()),
        backfill_cycle_mean_us: mean(microseconds(backfill.cycle_sum), backfill.cycles),
        backfilled_jobs: Some(backfill.backfilled_jobs.into()),
        rpcs,
    }
}

fn microseconds(time: std::time::Duration) -> u64 {
    u64::try_from(time.as_micros()).unwrap_or(u64::MAX)
}

/// The mean of a sum over `count` cycles, or `None` before the first cycle.
fn mean(sum: u64, count: u32) -> Option<u64> {
    (count > 0).then(|| sum / u64::from(count))
}

/// `rpc_num2string`.
fn rpc_name(kind: u16) -> String {
    RPCS.binary_search_by_key(&kind, |&(number, _)| number)
        .map_or_else(|_| kind.to_string(), |i| RPCS[i].1.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpcs_spell_like_sdiag() {
        assert_eq!(rpc_name(1008), "REQUEST_PING");
        assert_eq!(rpc_name(2003), "REQUEST_JOB_INFO");
        assert_eq!(rpc_name(1007), "1007");
    }

    #[test]
    fn a_mean_needs_a_cycle() {
        assert_eq!(mean(900, 3), Some(300));
        assert_eq!(mean(0, 0), None);
    }
}
