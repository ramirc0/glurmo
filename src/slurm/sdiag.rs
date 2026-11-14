//! Scheduler and RPC counters from sdiag.

use anyhow::{Context, Result};
use serde::Serialize;

use super::fields::integer;

/// The command.
pub fn argv() -> Vec<String> {
    vec!["sdiag".to_owned()]
}

/// Counters since the controller's last daily reset. A counter this Slurm version does not print is `None`.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
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
    /// The main scheduler's mean cycle.
    pub main_cycle_mean_us: Option<u64>,
    /// Jobs the main scheduler looked at in its last cycle.
    pub main_queue_last: Option<u64>,
    /// The backfill scheduler's mean cycle.
    pub backfill_cycle_mean_us: Option<u64>,
    /// Jobs started by backfill since the controller started.
    pub backfilled_jobs: Option<u64>,
    /// RPCs by message type.
    pub rpcs: Vec<RpcCount>,
}

/// How often the controller served one message type, and for how long in total.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RpcCount {
    /// The message type, such as `REQUEST_JOB_INFO`.
    pub kind: String,
    /// Calls served.
    pub count: u64,
    /// Time spent serving them.
    pub total_us: u64,
}

/// Parses the output of [`argv`].
///
/// # Errors
///
/// Fails on a malformed row in the table of RPCs by message type.
pub fn parse(stdout: &str) -> Result<Stats> {
    let mut stats = Stats::default();
    let mut section = "";
    for line in stdout.lines() {
        let Some(item) = line.strip_prefix('\t') else {
            match number(line) {
                Some(("Server thread count", value)) => stats.server_threads = Some(value),
                Some(("Agent queue size", value)) => stats.agent_queue = Some(value),
                Some(("DBD Agent queue size", value)) => stats.dbd_queue = Some(value),
                Some(("Jobs submitted", value)) => stats.jobs_submitted = Some(value),
                Some(("Jobs started", value)) => stats.jobs_started = Some(value),
                Some(("Jobs completed", value)) => stats.jobs_completed = Some(value),
                Some(_) => {}
                None => section = line,
            }
            continue;
        };
        if section.starts_with("Remote Procedure Call statistics by message type") {
            stats
                .rpcs
                .push(rpc(item).with_context(|| format!("sdiag row {line:?}"))?);
        } else if section.starts_with("Main schedule statistics") {
            match number(item) {
                Some(("Mean cycle", value)) => stats.main_cycle_mean_us = Some(value),
                Some(("Last queue length", value)) => stats.main_queue_last = Some(value),
                _ => {}
            }
        } else if section.starts_with("Backfilling stats") {
            match number(item) {
                Some(("Mean cycle", value)) => stats.backfill_cycle_mean_us = Some(value),
                Some(("Total backfilled jobs (since last slurm start)", value)) => {
                    stats.backfilled_jobs = Some(value);
                }
                _ => {}
            }
        }
    }
    Ok(stats)
}

/// Splits `Key: 123`. Anything else, such as a section header, gives `None`.
fn number(line: &str) -> Option<(&str, u64)> {
    let (key, value) = line.split_once(':')?;
    Some((key.trim(), value.trim().parse().ok()?))
}

/// Parses `REQUEST_PING   ( 1008) count:66588  ave_time:212    total_time:14149021`.
fn rpc(row: &str) -> Result<RpcCount> {
    let kind = row.split_whitespace().next().context("empty row")?;
    let field = |name: &str| -> Result<u64> {
        let value = row
            .split_whitespace()
            .find_map(|word| word.strip_prefix(name)?.strip_prefix(':'))
            .with_context(|| format!("no {name}"))?;
        integer(value)
    };
    Ok(RpcCount {
        kind: kind.to_owned(),
        count: field("count")?,
        total_us: field("total_time")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = "\
*******************************************************
sdiag output at Sat Nov 14 17:07:50 2026 (1794694070)
Data since      Fri Nov 13 19:00:00 2026 (1794614400)
*******************************************************
Server thread count:  2
Agent queue size:     0
Agent count:          0
Agent thread count:   0
DBD Agent queue size: 0

Jobs submitted: 20622
Jobs started:   21579
Jobs completed: 21610
Jobs canceled:  54
Jobs failed:    0

Main schedule statistics (microseconds):
\tLast cycle:   271
\tMax cycle:    91442
\tTotal cycles: 27812
\tMean cycle:   13713
\tMean depth cycle:  205
\tCycles per minute: 20
\tLast queue length: 1

Backfilling stats (WARNING: data obtained in the middle of backfilling execution.)
\tTotal backfilled jobs (since last slurm start): 1856
\tTotal backfilled jobs (since last stats cycle start): 1774
\tTotal cycles: 664
\tLast cycle when: Sat Nov 14 17:07:34 2026 (1794694054)
\tLast cycle: 633
\tMean cycle: 21804
\tLast queue length: 1

Remote Procedure Call statistics by message type
\tREQUEST_PING                            ( 1008) count:66588  ave_time:212    total_time:14149021
\tREQUEST_JOB_INFO                        ( 2003) count:4255   ave_time:12117  total_time:51559797
\tREQUEST_JOB_INFO_SINGLE                 ( 2021) count:197    ave_time:418    total_time:82539

Remote Procedure Call statistics by user
\totter           (   20001) count:286522 ave_time:2794   total_time:800594690

Pending RPC statistics
\tNo pending RPCs
";

    #[test]
    fn counters_by_section() {
        let stats = parse(LIVE).unwrap();
        assert_eq!(stats.server_threads, Some(2));
        assert_eq!(stats.jobs_started, Some(21_579));
        assert_eq!(stats.main_cycle_mean_us, Some(13_713));
        assert_eq!(stats.main_queue_last, Some(1));
        assert_eq!(stats.backfill_cycle_mean_us, Some(21_804));
        assert_eq!(stats.backfilled_jobs, Some(1856));
    }

    #[test]
    fn rpcs_by_message_type_only() {
        let stats = parse(LIVE).unwrap();
        let kinds: Vec<&str> = stats.rpcs.iter().map(|rpc| rpc.kind.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "REQUEST_PING",
                "REQUEST_JOB_INFO",
                "REQUEST_JOB_INFO_SINGLE"
            ]
        );
        assert_eq!(
            stats.rpcs[1],
            RpcCount {
                kind: "REQUEST_JOB_INFO".to_owned(),
                count: 4255,
                total_us: 51_559_797
            }
        );
    }

    #[test]
    fn missing_counters_stay_none() {
        let stats = parse("Server thread count: 3\n").unwrap();
        assert_eq!(stats.server_threads, Some(3));
        assert_eq!(stats.backfilled_jobs, None);
    }
}
