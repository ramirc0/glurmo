//! Turns each tick's listings into the events that changed since the last tick.

use std::collections::HashMap;

use crate::model::{
    Attempt, AttemptKey, EndState, Event, JobId, JobState, Node, Partition, UnixTime,
};
use crate::slurm::sacct::Outcome;
use crate::slurm::scontrol::Placed;

/// What the sampler has seen so far.
#[derive(Default)]
pub struct Known {
    /// When the first listing came in. Records that ended before it are history and fire nothing.
    since: Option<UnixTime>,
    /// When the last listing came in.
    at: UnixTime,
    attempts: HashMap<AttemptKey, Seen>,
    queue: Vec<Attempt>,
    nodes: Vec<Node>,
    partitions: Vec<Partition>,
}

struct Seen {
    phase: Phase,
    /// When a listing last held it.
    listed: UnixTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Has never held cores.
    Waiting,
    /// Holds cores.
    Holding,
    /// Has freed its cores, and how it ended is not known yet.
    Ended,
    /// Nothing more to report.
    Settled,
}

impl Known {
    /// Compares a full job listing taken at `at` with the last one.
    ///
    /// An attempt that held cores and is missing from the listing waits in [`Known::unsettled`]
    /// until [`Known::settle`] learns how it ended, unless a newer submit of the same job shows it was requeued.
    pub fn diff_jobs(&mut self, jobs: &[Attempt], at: UnixTime) -> Vec<Event> {
        let first = self.since.is_none();
        self.since.get_or_insert(at);
        self.at = at;
        let mut events = Vec::new();
        for job in jobs {
            let was = self.attempts.get(&job.key).map(|seen| seen.phase);
            let phase = next_phase(was, job, first, at, &mut events);
            self.attempts.insert(job.key, Seen { phase, listed: at });
        }

        let mut resubmitted: HashMap<JobId, UnixTime> = HashMap::new();
        for job in jobs {
            let submit = resubmitted.entry(job.key.job).or_insert(job.key.submit);
            *submit = (*submit).max(job.key.submit);
        }
        self.attempts.retain(|key, seen| {
            if seen.listed == at {
                return true;
            }
            match (seen.phase, resubmitted.get(&key.job)) {
                (Phase::Waiting | Phase::Settled, _) => false,
                (phase, Some(&submit)) if submit > key.submit => {
                    events.push(finish(*key, phase, submit, EndState::Requeued));
                    false
                }
                _ => true,
            }
        });

        let queue: Vec<Attempt> = jobs
            .iter()
            .filter(|job| job.state == JobState::Pending)
            .cloned()
            .collect();
        if queue != self.queue {
            self.queue.clone_from(&queue);
            events.push(Event::Queue(queue));
        }
        events
    }

    /// Jobs that left the listing before showing how they ended, in ID order.
    pub fn unsettled(&self) -> Vec<JobId> {
        let mut jobs: Vec<JobId> = self
            .attempts
            .iter()
            .filter(|(_, seen)| seen.listed < self.at)
            .map(|(key, _)| key.job)
            .collect();
        jobs.sort_unstable();
        jobs.dedup();
        jobs
    }

    /// Ends every unsettled attempt from what accounting holds.
    /// An attempt that accounting lacks, or still lists as running, ends as [`EndState::Lost`] when it was last listed.
    pub fn settle(&mut self, outcomes: &[Outcome]) -> Vec<Event> {
        let mut events = Vec::new();
        let at = self.at;
        self.attempts.retain(|key, seen| {
            if seen.listed == at {
                return true;
            }
            let outcome = outcomes.iter().find(|outcome| outcome.key == *key);
            let (end, state) = match outcome {
                Some(&Outcome {
                    end: Some(end),
                    state: JobState::Ended(state),
                    ..
                }) => (end, state),
                _ => (seen.listed, EndState::Lost),
            };
            events.push(finish(*key, seen.phase, end, state));
            false
        });
        events
    }

    /// Keeps a node listing, and returns it as an event when it changed.
    pub fn diff_nodes(&mut self, nodes: Vec<Node>) -> Option<Event> {
        (nodes != self.nodes).then(|| {
            self.nodes.clone_from(&nodes);
            Event::Nodes(nodes)
        })
    }

    /// Keeps a partition listing, and returns it as an event when it changed.
    pub fn diff_partitions(&mut self, partitions: Vec<Partition>) -> Option<Event> {
        (partitions != self.partitions).then(|| {
            self.partitions.clone_from(&partitions);
            Event::Partitions(partitions)
        })
    }

    /// The last node listing.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
}

/// Moves one listed job along, and records the events that takes.
fn next_phase(
    was: Option<Phase>,
    job: &Attempt,
    first: bool,
    at: UnixTime,
    events: &mut Vec<Event>,
) -> Phase {
    let started = || Event::Started {
        attempt: job.clone(),
    };
    let ended = |state| Event::Ended {
        key: job.key,
        at: job.end.unwrap_or(at),
        state,
    };
    match (was, job.state) {
        (None | Some(Phase::Waiting), JobState::Pending) => Phase::Waiting,
        (None | Some(Phase::Waiting), JobState::Running | JobState::Suspended) => {
            events.push(started());
            Phase::Holding
        }
        (None, JobState::Completing | JobState::Ended(_)) if first => Phase::Settled,
        (None | Some(Phase::Waiting), JobState::Ended(_)) if job.start.is_none() => {
            events.push(Event::Withdrawn {
                attempt: job.clone(),
            });
            Phase::Settled
        }
        (None | Some(Phase::Waiting), JobState::Completing) if job.start.is_none() => {
            Phase::Settled
        }
        (None | Some(Phase::Waiting), JobState::Completing) => {
            events.extend([started(), ended(None)]);
            Phase::Ended
        }
        (None | Some(Phase::Waiting), JobState::Ended(state)) => {
            events.extend([started(), ended(Some(state))]);
            Phase::Settled
        }
        (Some(Phase::Holding), JobState::Running | JobState::Suspended) => Phase::Holding,
        (Some(Phase::Holding), JobState::Pending) => {
            events.push(ended(Some(EndState::Requeued)));
            Phase::Waiting
        }
        (Some(Phase::Holding), JobState::Completing) => {
            events.push(ended(None));
            Phase::Ended
        }
        (Some(Phase::Holding), JobState::Ended(state)) => {
            events.push(ended(Some(state)));
            Phase::Settled
        }
        (Some(Phase::Ended), JobState::Ended(state)) => {
            events.push(Event::Settled {
                key: job.key,
                state,
            });
            Phase::Settled
        }
        (Some(phase @ (Phase::Ended | Phase::Settled)), _) => phase,
    }
}

/// The event that closes an attempt: its end while it still holds cores, or else its final state.
fn finish(key: AttemptKey, phase: Phase, at: UnixTime, state: EndState) -> Event {
    if phase == Phase::Holding {
        Event::Ended {
            key,
            at,
            state: Some(state),
        }
    } else {
        Event::Settled { key, state }
    }
}

/// A lookup's placement for `attempt`. It counts only when its job ID and start time match,
/// because a lookup can race a requeue.
pub fn placed(attempt: &Attempt, found: Placed) -> Option<Event> {
    (found.job == attempt.key.job && found.start == attempt.start).then_some(Event::Placed {
        key: attempt.key,
        placement: found.placement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ArrayTasks, Login, NodeCells, NodeName, PartitionName, Placement};

    const SUBMIT: UnixTime = UnixTime(1_000);

    fn job(id: u32, state: JobState) -> Attempt {
        let ran = !matches!(state, JobState::Pending);
        let ended = matches!(state, JobState::Completing | JobState::Ended(_));
        Attempt {
            key: AttemptKey {
                job: JobId(id),
                submit: SUBMIT,
            },
            array: None,
            login: Login("otter".into()),
            partitions: vec![PartitionName("4hours".into())],
            state,
            state_raw: String::new(),
            cpus: 4,
            mem_mb: None,
            gpus: 0,
            time_limit_s: None,
            start: ran.then_some(UnixTime(1_100)),
            end: ended.then_some(UnixTime(1_190)),
            nodes: if ran {
                vec![NodeName("z1".into())]
            } else {
                Vec::new()
            },
            reason: None,
            name: "job".into(),
        }
    }

    fn key(id: u32) -> AttemptKey {
        AttemptKey {
            job: JobId(id),
            submit: SUBMIT,
        }
    }

    const RUNNING: JobState = JobState::Running;
    const PENDING: JobState = JobState::Pending;
    const COMPLETING: JobState = JobState::Completing;
    const COMPLETED: JobState = JobState::Ended(EndState::Completed);

    /// Diffs listings 100 s apart, starting from a cold start at 1,000 s, and returns the last tick's events.
    fn ticks(listings: &[&[Attempt]]) -> (Known, Vec<Event>) {
        let mut known = Known::default();
        let mut events = Vec::new();
        for (i, jobs) in listings.iter().enumerate() {
            events = known.diff_jobs(jobs, UnixTime(1_000 + 100 * i64::try_from(i).unwrap()));
        }
        (known, events)
    }

    fn without_queue(events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .filter(|event| !matches!(event, Event::Queue(_)))
            .collect()
    }

    #[test]
    fn cold_start_starts_running_jobs_and_keeps_ended_ones_as_history() {
        let jobs = [
            job(1, RUNNING),
            job(2, COMPLETED),
            job(3, COMPLETING),
            job(4, PENDING),
        ];
        let (_, events) = ticks(&[&jobs]);
        assert_eq!(
            events,
            [
                Event::Started {
                    attempt: jobs[0].clone()
                },
                Event::Queue(vec![jobs[3].clone()]),
            ]
        );
    }

    #[test]
    fn pending_job_starts() {
        let (_, events) = ticks(&[&[job(1, PENDING)], &[job(1, RUNNING)]]);
        assert_eq!(
            events,
            [
                Event::Started {
                    attempt: job(1, RUNNING)
                },
                Event::Queue(vec![]),
            ]
        );
    }

    #[test]
    fn completing_frees_cores_at_once_and_settles_later() {
        let running: &[Attempt] = &[job(1, RUNNING)];
        let completing: &[Attempt] = &[job(1, COMPLETING)];
        let completed: &[Attempt] = &[job(1, COMPLETED)];
        let ended = Event::Ended {
            key: key(1),
            at: UnixTime(1_190),
            state: None,
        };
        assert_eq!(ticks(&[running, completing]).1, [ended]);
        assert_eq!(
            ticks(&[running, completing, completed]).1,
            [Event::Settled {
                key: key(1),
                state: EndState::Completed
            }]
        );
    }

    #[test]
    fn job_that_ends_outright_fires_its_state() {
        let failed = job(1, JobState::Ended(EndState::Failed));
        assert_eq!(
            ticks(&[&[job(1, RUNNING)], &[failed]]).1,
            [Event::Ended {
                key: key(1),
                at: UnixTime(1_190),
                state: Some(EndState::Failed)
            }]
        );
    }

    #[test]
    fn job_that_starts_and_ends_between_polls_fires_both() {
        let (_, events) = ticks(&[&[job(1, PENDING)], &[job(1, COMPLETED)]]);
        assert_eq!(
            without_queue(events),
            [
                Event::Started {
                    attempt: job(1, COMPLETED)
                },
                Event::Ended {
                    key: key(1),
                    at: UnixTime(1_190),
                    state: Some(EndState::Completed)
                },
            ]
        );
        let (_, events) = ticks(&[&[], &[job(2, COMPLETED)]]);
        assert_eq!(
            events.len(),
            2,
            "a job first seen ended after the cold start"
        );
    }

    #[test]
    fn job_cancelled_while_pending_is_withdrawn() {
        let mut cancelled = job(1, JobState::Ended(EndState::Cancelled));
        cancelled.start = None;
        let withdrawn = Event::Withdrawn {
            attempt: cancelled.clone(),
        };
        let (_, events) = ticks(&[&[job(1, PENDING)], &[cancelled.clone()]]);
        assert_eq!(events, [withdrawn.clone(), Event::Queue(vec![])]);
        assert_eq!(
            ticks(&[&[], &[cancelled.clone()]]).1,
            [withdrawn],
            "submitted and cancelled between polls"
        );
        assert_eq!(ticks(&[&[cancelled]]).1, [], "history at a cold start");
    }

    fn array(id: u32, base: u32, spec: &str, count: u32, state: JobState) -> Attempt {
        Attempt {
            array: Some(ArrayTasks {
                job: JobId(base),
                spec: spec.into(),
                count,
            }),
            ..job(id, state)
        }
    }

    #[test]
    fn array_tasks_split_off_the_pending_range_and_the_last_reuses_its_id() {
        let mut known = Known::default();
        known.diff_jobs(&[array(10, 10, "0-1", 2, PENDING)], UnixTime(1_000));

        let first = array(11, 10, "0", 1, RUNNING);
        let events = known.diff_jobs(
            &[first.clone(), array(10, 10, "1", 1, PENDING)],
            UnixTime(1_100),
        );
        assert_eq!(
            without_queue(events),
            [Event::Started {
                attempt: first.clone()
            }]
        );

        let last = array(10, 10, "1", 1, RUNNING);
        let events = known.diff_jobs(&[first, last.clone()], UnixTime(1_200));
        assert_eq!(
            events,
            [Event::Started { attempt: last }, Event::Queue(vec![])]
        );
    }

    #[test]
    fn requeue_ends_the_old_attempt_at_the_new_submit() {
        let mut requeued = job(1, PENDING);
        requeued.key.submit = UnixTime(1_150);
        let (known, events) = ticks(&[&[job(1, RUNNING)], &[requeued.clone()]]);
        assert_eq!(
            events,
            [
                Event::Ended {
                    key: key(1),
                    at: UnixTime(1_150),
                    state: Some(EndState::Requeued)
                },
                Event::Queue(vec![requeued]),
            ]
        );
        assert_eq!(known.unsettled(), []);
    }

    fn outcome(id: u32, end: Option<i64>, state: JobState) -> Outcome {
        Outcome {
            key: key(id),
            end: end.map(UnixTime),
            state,
        }
    }

    #[test]
    fn vanished_jobs_wait_for_accounting() {
        let jobs = [job(1, RUNNING), job(2, RUNNING), job(3, RUNNING)];
        let (mut known, events) = ticks(&[&jobs, &[]]);
        assert_eq!(events, []);
        assert_eq!(known.unsettled(), [JobId(1), JobId(2), JobId(3)]);

        let events = known.settle(&[
            outcome(1, Some(1_150), JobState::Ended(EndState::Timeout)),
            outcome(2, None, RUNNING),
        ]);
        let mut ends: Vec<(AttemptKey, UnixTime, Option<EndState>)> = events
            .into_iter()
            .map(|event| match event {
                Event::Ended { key, at, state } => (key, at, state),
                other => panic!("{other:?}"),
            })
            .collect();
        ends.sort_by_key(|(key, ..)| *key);
        assert_eq!(
            ends,
            [
                (key(1), UnixTime(1_150), Some(EndState::Timeout)),
                (key(2), UnixTime(1_000), Some(EndState::Lost)),
                (key(3), UnixTime(1_000), Some(EndState::Lost)),
            ]
        );
        assert_eq!(known.unsettled(), []);
    }

    #[test]
    fn vanished_completing_job_settles_from_accounting() {
        let (mut known, _) = ticks(&[&[job(1, RUNNING)], &[job(1, COMPLETING)], &[]]);
        assert_eq!(known.unsettled(), [JobId(1)]);
        assert_eq!(
            known.settle(&[outcome(1, Some(1_190), COMPLETED)]),
            [Event::Settled {
                key: key(1),
                state: EndState::Completed
            }]
        );
    }

    #[test]
    fn queue_fires_only_when_it_changes() {
        let pending: &[Attempt] = &[job(1, PENDING)];
        let (mut known, events) = ticks(&[pending]);
        assert_eq!(events, [Event::Queue(pending.to_vec())]);
        assert_eq!(known.diff_jobs(pending, UnixTime(1_100)), []);
    }

    #[test]
    fn nodes_and_partitions_fire_only_when_they_change() {
        let mut known = Known::default();
        assert_eq!(known.diff_nodes(Vec::new()), None);
        let partitions = vec![crate::model::Partition {
            name: PartitionName("4hours".into()),
            nodes: vec![NodeName("z1".into())],
            max_time_s: Some(14_400),
            default: true,
            hidden: false,
            state_raw: "UP".into(),
        }];
        assert_eq!(
            known.diff_partitions(partitions.clone()),
            Some(Event::Partitions(partitions.clone()))
        );
        assert_eq!(known.diff_partitions(partitions), None);
    }

    #[test]
    fn lookup_counts_only_when_job_and_start_match() {
        let attempt = job(1, RUNNING);
        let found = |start| Placed {
            job: JobId(1),
            start: Some(UnixTime(start)),
            state: RUNNING,
            placement: Placement(vec![NodeCells {
                node: NodeName("z1".into()),
                cores: vec![0, 1],
                unplaced: 0,
                gpus: vec![],
                mem_mb: None,
            }]),
        };
        assert!(matches!(
            placed(&attempt, found(1_100)),
            Some(Event::Placed { key: placed_key, .. }) if placed_key == key(1)
        ));
        assert_eq!(placed(&attempt, found(1_500)), None);
    }
}
