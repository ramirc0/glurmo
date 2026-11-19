//! Scheduler and RPC counters, from `RESPONSE_STATS_INFO`, as sdiag reads them.

use anyhow::{Result, bail};

use super::Protocol;
use super::pack::{Pack, Unpack};
use super::rpcs::RPCS;
use glurmo_model::{RpcCount, Stats};

/// The request.
pub const REQUEST: u16 = 2035;
/// The reply.
pub const RESPONSE: u16 = 2036;

const GET: u16 = 1;

/// The body of the request.
pub fn request() -> Vec<u8> {
    let mut body = Pack::default();
    body.u16(GET);
    body.0
}

/// Decodes a `RESPONSE_STATS_INFO` body. RPCs come busiest first, as sdiag lists them.
///
/// # Errors
///
/// Fails when the body does not follow `protocol`'s layout.
pub fn decode(body: &[u8], protocol: Protocol) -> Result<Stats> {
    let u = &mut Unpack::new(body);
    let mut stats = Stats::default();
    let v25_11 = protocol == Protocol::V25_11;
    if v25_11 || u.u32()? != 0 {
        let _req_time = (u.time()?, u.time()?);
        stats.server_threads = Some(u.u32()?.into());
        stats.agent_queue = Some(u.u32()?.into());
        let _agents = (u.u32()?, u.u32()?);
        stats.dbd_queue = Some(u.u32()?.into());
        let _gettimeofday_latency = u.u32()?;
        stats.jobs_submitted = Some(u.u32()?.into());
        stats.jobs_started = Some(u.u32()?.into());
        stats.jobs_completed = Some(u.u32()?.into());
        let _jobs_canceled_failed_pending_running = (u.u32()?, u.u32()?, u.u32()?, u.u32()?);
        let _job_states_ts = u.time()?;
        let _schedule_cycle_max_last = (u.u32()?, u.u32()?);
        let cycle_sum = if v25_11 {
            u.u64()?
        } else {
            u64::from(u.u32()?)
        };
        let cycles = u.u32()?;
        let _schedule_cycle_depth = u.u32()?;
        let _schedule_exit = u.u32_array()?;
        stats.main_queue_last = Some(u.u32()?.into());
        stats.main_cycle_mean_us = (cycles > 0).then(|| cycle_sum / u64::from(cycles));
        stats.backfilled_jobs = Some(u.u32()?.into());
        let _bf_last_backfilled_jobs = u.u32()?;
        let bf_cycles = u.u32()?;
        let bf_cycle_sum = u.u64()?;
        stats.backfill_cycle_mean_us = (bf_cycles > 0).then(|| bf_cycle_sum / u64::from(bf_cycles));
        let _bf_cycle_last_depth_try_queue_max = (u.u32()?, u.u32()?, u.u32()?, u.u32()?, u.u32()?);
        let _bf_when_last_cycle = u.time()?;
        for _sum in 0..7 {
            u.u32()?;
        }
        let _bf_exit = u.u32_array()?;
    }
    let _rpc_type_size = u.u32()?;
    let kinds = u.u16_array()?;
    let counts = u.u32_array()?;
    let times = u.u64_array()?;
    if kinds.len() != counts.len() || kinds.len() != times.len() {
        bail!("RPC arrays differ in length");
    }
    if v25_11 && u.u8()? != 0 {
        let _queued = u.u16_array()?;
        let _dropped = u.u64_array()?;
        let _cycle_last_max = (u.u16_array()?, u.u16_array()?);
    }
    let _rpc_user_size = u.u32()?;
    let _users = (u.u32_array()?, u.u32_array()?, u.u64_array()?);
    let _pending_by_type = (u.u32_array()?, u.u32_array()?);
    let _dumps = (u.u32_array()?, u.str_array()?);
    u.finish()?;
    stats.rpcs = kinds
        .iter()
        .zip(&counts)
        .zip(&times)
        .map(|((&kind, &count), &total_us)| RpcCount {
            kind: rpc_name(kind),
            count: count.into(),
            total_us,
        })
        .collect();
    stats.rpcs.sort_by_key(|rpc| std::cmp::Reverse(rpc.count));
    Ok(stats)
}

/// `rpc_num2string`.
fn rpc_name(kind: u16) -> String {
    RPCS.binary_search_by_key(&kind, |&(number, _)| number)
        .map_or_else(|_| kind.to_string(), |i| RPCS[i].1.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 23.11 reply without the scheduler parts: one RPC type, one user, no queues, no dumps.
    fn rpcs_only() -> Pack {
        let mut body = Pack::default();
        body.u32(0);
        body.u32(1).u32(1).u16(2003).u32(1).u32(7).u32(1).u64(900);
        body.u32(1).u32(1).u32(0).u32(1).u32(7).u32(1).u64(900);
        body.u32(0).u32(0);
        body.u32(0).u32(0);
        body
    }

    #[test]
    fn a_reply_is_read_to_its_end() {
        let stats = decode(&rpcs_only().0, Protocol::V23_11).unwrap();
        assert_eq!(stats.rpcs[0].kind, "REQUEST_JOB_INFO");
        assert_eq!((stats.rpcs[0].count, stats.rpcs[0].total_us), (7, 900));
        let mut longer = rpcs_only();
        longer.u16(0);
        assert!(decode(&longer.0, Protocol::V23_11).is_err());
    }

    #[test]
    fn rpcs_spell_like_sdiag() {
        assert_eq!(rpc_name(1008), "REQUEST_PING");
        assert_eq!(rpc_name(2003), "REQUEST_JOB_INFO");
        assert_eq!(rpc_name(1007), "1007");
    }

    /// A reply with the scheduler parts, and a distinct value in each field glurmo reads.
    fn with_scheduler(protocol: Protocol) -> Pack {
        let v25_11 = protocol == Protocol::V25_11;
        let mut body = Pack::default();
        if !v25_11 {
            body.u32(1);
        }
        body.time(0).time(0);
        body.u32(11).u32(12).u32(0).u32(0).u32(13).u32(0);
        body.u32(14).u32(15).u32(16);
        body.u32(0).u32(0).u32(0).u32(0).time(0).u32(0).u32(0);
        if v25_11 {
            body.u64(900);
        } else {
            body.u32(900);
        }
        body.u32(3).u32(0).u32(0).u32(17);
        body.u32(18).u32(0).u32(4).u64(2_000);
        body.u32(0).u32(0).u32(0).u32(0).u32(0).time(0);
        for _sum in 0..7 {
            body.u32(0);
        }
        body.u32(0);
        body.u32(2).u32(2).u16(2003).u16(1008);
        body.u32(2).u32(5).u32(9).u32(2).u64(700).u64(300);
        if v25_11 {
            body.0.push(0);
        }
        for _empty in 0..8 {
            body.u32(0);
        }
        body
    }

    #[test]
    fn every_counter_lands_in_its_own_field() {
        let rpc = |kind: &str, count, total_us| RpcCount {
            kind: kind.into(),
            count,
            total_us,
        };
        let expected = Stats {
            server_threads: Some(11),
            agent_queue: Some(12),
            dbd_queue: Some(13),
            jobs_submitted: Some(14),
            jobs_started: Some(15),
            jobs_completed: Some(16),
            main_cycle_mean_us: Some(300),
            main_queue_last: Some(17),
            backfill_cycle_mean_us: Some(500),
            backfilled_jobs: Some(18),
            rpcs: vec![rpc("REQUEST_PING", 9, 300), rpc("REQUEST_JOB_INFO", 5, 700)],
        };
        for protocol in Protocol::NEWEST_FIRST {
            let stats = decode(&with_scheduler(protocol).0, protocol).unwrap();
            assert_eq!(stats, expected, "{protocol:?}");
        }
    }
}
