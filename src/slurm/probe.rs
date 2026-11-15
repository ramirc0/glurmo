//! What a new cluster offers, what each command costs, and how often to run it.

use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use super::recording::Call;
use super::{Intervals, Lookup, Site, Slurm};
use glurmo_model::{JobState, RpcCount, Stats};

/// Runs per command. The minimum delta over them filters out other users' RPCs.
const RUNS: usize = 3;

const JOBS: &str = "jobs";
const NODES: &str = "nodes";

/// What the probe found.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Site facts.
    pub site: Site,
    /// What this user can see.
    pub capabilities: Capabilities,
    /// Each command's latency and controller cost.
    pub costs: Vec<Cost>,
    /// Sampling intervals that keep the controller load low.
    pub suggested: Intervals,
}

/// What this user can see. `None` means the probe had no job to try.
#[derive(Debug, Serialize)]
pub struct Capabilities {
    /// Other users' jobs show in squeue.
    pub sees_all_jobs: bool,
    /// sinfo works, so per-node views do.
    pub sees_nodes: bool,
    /// Lookups give CPU and GPU indices.
    pub exact_placement: Option<bool>,
    /// sacct shows other users' jobs.
    pub history: Option<bool>,
    /// sdiag works.
    pub scheduler_stats: bool,
}

/// One command's latency and the RPCs it cost the controller.
#[derive(Debug, Serialize)]
pub struct Cost {
    /// What the command does.
    pub label: &'static str,
    /// The median wall time.
    pub latency_ms: u64,
    /// RPCs by message type, per call. Empty when sdiag is unavailable.
    pub rpcs: Vec<RpcCount>,
}

/// Probes the cluster behind `slurm`.
///
/// # Errors
///
/// Fails when a call fails while it is being timed.
pub async fn probe(slurm: &mut Slurm) -> Result<Report> {
    let jobs = slurm.jobs().await.unwrap_or_default();
    let sees_nodes = slurm.nodes().await.is_ok();
    let running = jobs.iter().find(|job| job.state == JobState::Running);
    let scheduler_stats = slurm.stats().await.is_ok();

    let mut calls: Vec<(&'static str, Call)> = vec![
        (JOBS, Call::Jobs),
        (NODES, Call::Nodes),
        ("placement, every job", Call::Placements),
        ("partitions", Call::Partitions),
        ("configuration", Call::Config),
    ];
    let mut exact_placement = None;
    if let Some(job) = running {
        let lookup = Call::Lookup {
            key: job.key,
            printed: job.printed_id(),
        };
        calls.insert(2, ("placement, one job", lookup));
        if let Ok(Lookup::Found(placed)) = slurm.lookup(job).await {
            exact_placement = Some(!placed.placement.0.is_empty());
        }
    }
    let mut costs = Vec::new();
    for (label, call) in calls {
        costs.push(cost(slurm, label, &call, scheduler_stats).await?);
    }
    let history = match running.or(jobs.first()) {
        Some(job) => Some(
            slurm
                .outcomes(&[job.key.job])
                .await
                .is_ok_and(|outcomes| outcomes.iter().any(|o| o.key.job == job.key.job)),
        ),
        None => None,
    };
    let site = slurm.site().clone();
    let latency = |label| {
        costs
            .iter()
            .find(|cost| cost.label == label)
            .map_or(0, |cost| cost.latency_ms)
    };
    let suggested = suggest(&site, latency(JOBS), latency(NODES));
    Ok(Report {
        capabilities: Capabilities {
            sees_all_jobs: !site.private_data.iter().any(|item| item == "jobs"),
            sees_nodes,
            exact_placement,
            history,
            scheduler_stats,
        },
        site,
        costs,
        suggested,
    })
}

/// Times a call and measures its RPCs between stats snapshots.
async fn cost(
    slurm: &mut Slurm,
    label: &'static str,
    call: &Call,
    with_rpcs: bool,
) -> Result<Cost> {
    let mut latencies = Vec::new();
    let mut fewest: HashMap<String, RpcCount> = HashMap::new();
    for run in 0..RUNS {
        let before = if with_rpcs {
            slurm.stats().await?
        } else {
            Stats::default()
        };
        let started = Instant::now();
        slurm.ask(call.clone()).await?;
        latencies.push(started.elapsed());
        if !with_rpcs {
            continue;
        }
        let after = slurm.stats().await?;
        for rpc in rpc_deltas(&before, &after) {
            match fewest.get_mut(&rpc.kind) {
                Some(least) => {
                    least.count = least.count.min(rpc.count);
                    least.total_us = least.total_us.min(rpc.total_us);
                }
                // A type first seen after the first run was missing from it, so its minimum is zero.
                None if run == 0 => {
                    fewest.insert(rpc.kind.clone(), rpc);
                }
                None => {}
            }
        }
    }
    latencies.sort();
    let mut rpcs: Vec<RpcCount> = fewest.into_values().filter(|rpc| rpc.count > 0).collect();
    rpcs.sort_by_key(|rpc| std::cmp::Reverse(rpc.total_us));
    Ok(Cost {
        label,
        latency_ms: u64::try_from(latencies[RUNS / 2].as_millis()).unwrap_or(u64::MAX),
        rpcs,
    })
}

/// RPCs between two snapshots by message type, without the snapshots' own `REQUEST_STATS_INFO`.
/// A type missing from `before` started at zero.
fn rpc_deltas(before: &Stats, after: &Stats) -> Vec<RpcCount> {
    after
        .rpcs
        .iter()
        .filter(|rpc| rpc.kind != "REQUEST_STATS_INFO")
        .map(|rpc| {
            let old = before.rpcs.iter().find(|old| old.kind == rpc.kind);
            RpcCount {
                kind: rpc.kind.clone(),
                count: rpc.count - old.map_or(0, |old| old.count.min(rpc.count)),
                total_us: rpc.total_us - old.map_or(0, |old| old.total_us.min(rpc.total_us)),
            }
        })
        .collect()
}

/// Picks intervals that keep each command's duty cycle near 2%, and the jobs interval below
/// `MinJobAge / 2`, so no ended job leaves squeue unseen. Under a rate limit, lookups use at most
/// half the tokens that refill during one jobs interval.
fn suggest(site: &Site, squeue_ms: u64, sinfo_ms: u64) -> Intervals {
    let duty = |latency_ms: u64, floor: u64| floor.max(latency_ms * 50 / 1000);
    let jobs_s = duty(squeue_ms, 10).min((site.min_job_age_s / 2).max(1));
    let lookups = site.rate_limit.map_or(25, |limit| {
        let tokens =
            u64::from(limit.refill_rate) * jobs_s / u64::from(limit.refill_period_s.max(1));
        (tokens / 2).saturating_sub(2).min(25)
    });
    Intervals {
        jobs_s,
        nodes_s: duty(sinfo_ms, 30),
        partitions_s: 600,
        lookups,
        ..Intervals::default()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let site = &self.site;
        let private = if site.private_data.is_empty() {
            "none".to_owned()
        } else {
            site.private_data.join(",")
        };
        let rate = site.rate_limit.map_or_else(
            || "none".to_owned(),
            |limit| {
                format!(
                    "{} per {} s, bucket {}",
                    limit.refill_rate, limit.refill_period_s, limit.bucket_size
                )
            },
        );
        writeln!(f, "cluster       {} (Slurm {})", site.cluster, site.version)?;
        writeln!(f, "MinJobAge     {} s", site.min_job_age_s)?;
        writeln!(f, "PrivateData   {private}")?;
        writeln!(f, "rate limit    {rate}")?;
        let capabilities = &self.capabilities;
        let tried = |value: Option<bool>| value.map_or("untested", yes_no);
        writeln!(f, "\ncapabilities")?;
        writeln!(
            f,
            "  sees all jobs     {}",
            yes_no(capabilities.sees_all_jobs)
        )?;
        writeln!(f, "  sees nodes        {}", yes_no(capabilities.sees_nodes))?;
        writeln!(
            f,
            "  exact placement   {}",
            tried(capabilities.exact_placement)
        )?;
        writeln!(f, "  history           {}", tried(capabilities.history))?;
        writeln!(
            f,
            "  scheduler stats   {}",
            yes_no(capabilities.scheduler_stats)
        )?;
        writeln!(f, "\n{:<30} {:>7}  controller RPCs per call", "call", "ms")?;
        for cost in &self.costs {
            let rpcs: Vec<String> = cost
                .rpcs
                .iter()
                .map(|rpc| format!("{} ×{} ({} µs)", rpc.kind, rpc.count, rpc.total_us))
                .collect();
            writeln!(
                f,
                "{:<30} {:>7}  {}",
                cost.label,
                cost.latency_ms,
                rpcs.join(", ")
            )?;
        }
        let suggested = &self.suggested;
        writeln!(
            f,
            "\nsuggested     jobs {} s, nodes {} s, partitions {} s, lookups {} per tick",
            suggested.jobs_s, suggested.nodes_s, suggested.partitions_s, suggested.lookups
        )
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slurm::{RateLimit, Version};

    fn site(min_job_age_s: u64, rate_limit: Option<RateLimit>) -> Site {
        Site {
            cluster: "test".to_owned(),
            version: Version(23, 11, 4),
            min_job_age_s,
            private_data: Vec::new(),
            rate_limit,
        }
    }

    fn rpc(kind: &str, count: u64, total_us: u64) -> RpcCount {
        RpcCount {
            kind: kind.to_owned(),
            count,
            total_us,
        }
    }

    #[test]
    fn deltas_skip_sdiag_itself_and_count_new_types_from_zero() {
        let before = Stats {
            rpcs: vec![
                rpc("REQUEST_JOB_INFO", 10, 100),
                rpc("REQUEST_STATS_INFO", 5, 5),
            ],
            ..Stats::default()
        };
        let after = Stats {
            rpcs: vec![
                rpc("REQUEST_JOB_INFO", 11, 130),
                rpc("REQUEST_STATS_INFO", 6, 6),
                rpc("REQUEST_FED_INFO", 1, 2),
            ],
            ..Stats::default()
        };
        assert_eq!(
            rpc_deltas(&before, &after),
            [
                rpc("REQUEST_JOB_INFO", 1, 30),
                rpc("REQUEST_FED_INFO", 1, 2)
            ]
        );
    }

    #[test]
    fn fast_commands_keep_the_default_intervals() {
        let intervals = suggest(&site(300, None), 20, 20);
        assert_eq!(
            (intervals.jobs_s, intervals.nodes_s, intervals.lookups),
            (10, 30, 25)
        );
    }

    #[test]
    fn slow_squeue_stays_below_half_min_job_age() {
        let intervals = suggest(&site(300, None), 4000, 0);
        assert_eq!(intervals.jobs_s, 150);
    }

    #[test]
    fn rate_limits_shrink_the_lookup_budget() {
        let limit = RateLimit {
            bucket_size: 30,
            refill_rate: 2,
            refill_period_s: 1,
        };
        let intervals = suggest(&site(300, Some(limit)), 0, 0);
        assert_eq!(intervals.lookups, 8);
    }
}
