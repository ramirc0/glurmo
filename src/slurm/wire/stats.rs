//! Scheduler and RPC counters, from `RESPONSE_STATS_INFO`, as sdiag reads them.

use anyhow::{Result, bail};

use super::pack::{Pack, Unpack};
use super::rpcs::RPCS;
use glurmo_model::{RpcCount, Stats};

/// The request.
pub const REQUEST: u16 = 2035;

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
/// Fails when the body does not follow Slurm 23.11's layout.
pub fn decode(body: &[u8]) -> Result<Stats> {
    let u = &mut Unpack::new(body);
    let mut stats = Stats::default();
    if u.u32()? != 0 {
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
        let cycle_sum = u.u32()?;
        let cycles = u.u32()?;
        let _schedule_cycle_depth = u.u32()?;
        let _schedule_exit = u.u32_array()?;
        stats.main_queue_last = Some(u.u32()?.into());
        stats.main_cycle_mean_us = (cycles > 0).then(|| u64::from(cycle_sum / cycles));
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

    #[test]
    fn rpcs_spell_like_sdiag() {
        assert_eq!(rpc_name(1008), "REQUEST_PING");
        assert_eq!(rpc_name(2003), "REQUEST_JOB_INFO");
        assert_eq!(rpc_name(1007), "1007");
    }
}
