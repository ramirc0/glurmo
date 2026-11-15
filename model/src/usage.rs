//! Usage history: what the server's `/api/usage` returns, and how a graph samples it.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::UnixTime;

/// Usage summed over one step for one login or partition.
///
/// # Examples
///
/// ```
/// use glurmo_model::usage::Step;
///
/// let step: Step = serde_json::from_str(
///     r#"{"at": 0, "key": "otter", "core_s": 600, "gpu_s": 0, "wait_s": 0, "started": 1, "ended": 0}"#,
/// )
/// .unwrap();
/// assert_eq!(step.failed, None, "failures are counted per partition only");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The step's start.
    pub at: UnixTime,
    /// The login or partition.
    pub key: String,
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
    /// Ended attempts that failed. Counted per partition only, never per person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<u32>,
}

/// Usage by login over a stretch of time.
///
/// # Examples
///
/// ```
/// use glurmo_model::UnixTime;
/// use glurmo_model::usage::Usage;
///
/// let usage = Usage { from: UnixTime(0), to: UnixTime(3_600), step_s: 300, steps: Vec::new() };
/// assert!(usage.steps.is_empty());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// The start.
    pub from: UnixTime,
    /// The end.
    pub to: UnixTime,
    /// The length of each step.
    pub step_s: i64,
    /// Usage per step and login.
    pub steps: Vec<Step>,
}

/// A step that gives at most about 600 steps over `span_s`, in whole five-minute bins.
///
/// # Examples
///
/// ```
/// use glurmo_model::usage::step_for;
///
/// assert_eq!(step_for(86_400), 300);
/// assert_eq!(step_for(7 * 86_400), 1_200);
/// ```
pub fn step_for(span_s: i64) -> i64 {
    ((span_s / 600).max(300) + 299) / 300 * 300
}

/// One column of a usage graph: cores held by each login, biggest user overall first, and tasks waiting.
///
/// # Examples
///
/// ```
/// use glurmo_model::usage::Column;
///
/// let column = Column { cores: vec![("otter", 12), ("heron", 4)], waiting: 30 };
/// assert_eq!(column.cores.iter().map(|(_, cores)| cores).sum::<u64>(), 16);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Column<'a> {
    /// Mean cores held over the step, by login.
    pub cores: Vec<(&'a str, u64)>,
    /// Mean tasks waiting over the step.
    pub waiting: u64,
}

impl Usage {
    /// Samples the step under the middle of each of `count` columns over `from` to `from + span_s`.
    /// A column that no step covers is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::UnixTime;
    /// use glurmo_model::usage::{Step, Usage};
    ///
    /// let step = |at, key: &str, core_s| Step {
    ///     at: UnixTime(at), key: key.into(), core_s, gpu_s: 0, wait_s: 0, started: 0, ended: 0, failed: None,
    /// };
    /// let usage = Usage {
    ///     from: UnixTime(0),
    ///     to: UnixTime(600),
    ///     step_s: 300,
    ///     steps: vec![step(0, "heron", 300), step(0, "otter", 1_200), step(300, "otter", 600)],
    /// };
    /// let columns = usage.columns(0, 600, 2);
    /// assert_eq!(columns[0].cores, [("otter", 4), ("heron", 1)]);
    /// assert_eq!(columns[1].cores, [("otter", 2)]);
    /// ```
    pub fn columns(&self, from: i64, span_s: i64, count: u16) -> Vec<Column<'_>> {
        let step_s = u64::try_from(self.step_s.max(1)).unwrap_or(1);
        let mut by_time: BTreeMap<i64, Vec<(&str, u64)>> = BTreeMap::new();
        let mut wait_s: HashMap<i64, u64> = HashMap::new();
        let mut totals: HashMap<&str, u64> = HashMap::new();
        for step in &self.steps {
            let cores = by_time.entry(step.at.0).or_default();
            cores.push((&step.key, step.core_s / step_s));
            *wait_s.entry(step.at.0).or_default() += step.wait_s;
            *totals.entry(&step.key).or_default() += step.core_s;
        }
        let count = i64::from(count);
        (0..count)
            .map(|i| {
                let time = from + (2 * i + 1) * span_s / (2 * count);
                let Some((at, cores)) = by_time
                    .range(..=time)
                    .next_back()
                    .filter(|(at, _)| **at + self.step_s > time)
                else {
                    return Column::default();
                };
                let mut cores = cores.clone();
                cores.sort_by_key(|(login, _)| (std::cmp::Reverse(totals[login]), *login));
                Column {
                    cores,
                    waiting: wait_s[at] / step_s,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_sums_every_login_before_it_rounds() {
        let step = |key: &str| Step {
            at: UnixTime(0),
            key: key.into(),
            core_s: 0,
            gpu_s: 0,
            wait_s: 200,
            started: 0,
            ended: 0,
            failed: None,
        };
        let usage = Usage {
            from: UnixTime(0),
            to: UnixTime(300),
            step_s: 300,
            steps: vec![step("heron"), step("otter")],
        };
        assert_eq!(usage.columns(0, 300, 1)[0].waiting, 1);
    }

    #[test]
    fn steps_are_whole_bins_and_at_most_about_600() {
        assert_eq!(step_for(3_600), 300);
        assert_eq!(step_for(86_400), 300);
        assert_eq!(step_for(7 * 86_400), 1_200);
        assert_eq!(step_for(30 * 86_400), 4_500);
    }
}
