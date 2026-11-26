//! Usage history: what the server's `/api/usage` returns, and how a graph samples it.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::UnixTime;

/// Usage summed over one step for one login or partition. It travels as the array
/// `[at, key, core_s, wait_s]`, because a month of steps by login runs to many thousands.
///
/// # Examples
///
/// ```
/// use glurmo_model::usage::Step;
///
/// let step: Step = serde_json::from_str(r#"[0, "otter", 600, 30]"#).unwrap();
/// assert_eq!((step.key.as_str(), step.core_s, step.wait_s), ("otter", 600, 30));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    from = "(UnixTime, String, u64, u64)",
    into = "(UnixTime, String, u64, u64)"
)]
pub struct Step {
    /// The step's start.
    pub at: UnixTime,
    /// The login or partition.
    pub key: String,
    /// Core-seconds held.
    pub core_s: u64,
    /// Task-seconds spent pending.
    pub wait_s: u64,
}

impl From<(UnixTime, String, u64, u64)> for Step {
    fn from((at, key, core_s, wait_s): (UnixTime, String, u64, u64)) -> Self {
        Self {
            at,
            key,
            core_s,
            wait_s,
        }
    }
}

impl From<Step> for (UnixTime, String, u64, u64) {
    fn from(step: Step) -> Self {
        (step.at, step.key, step.core_s, step.wait_s)
    }
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

impl Column<'_> {
    /// This column with its logins stacked by `place`, lowest first, then by login. A place that never
    /// changes for a login, such as the order it was first seen in, keeps a graph's bands in one order
    /// however the graph is panned or resized.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::usage::Column;
    ///
    /// let column = Column { cores: vec![("otter", 12), ("heron", 4), ("newt", 1)], waiting: 0 };
    /// let stacked = column.stacked_by(|login| if login == "heron" { 0 } else { 1 });
    /// assert_eq!(stacked.cores, [("heron", 4), ("newt", 1), ("otter", 12)]);
    /// ```
    #[must_use]
    pub fn stacked_by(mut self, place: impl Fn(&str) -> u64) -> Self {
        self.cores
            .sort_by_cached_key(|(login, _)| (place(login), *login));
        self
    }
}

impl Usage {
    /// Core-seconds held by each login or partition in steps that start from `from` up to `to`.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::UnixTime;
    /// use glurmo_model::usage::{Step, Usage};
    ///
    /// let step = |at, key: &str, core_s| Step { at: UnixTime(at), key: key.into(), core_s, wait_s: 0 };
    /// let usage = Usage {
    ///     from: UnixTime(0),
    ///     to: UnixTime(900),
    ///     step_s: 300,
    ///     steps: vec![step(0, "heron", 300), step(300, "otter", 600), step(600, "otter", 600)],
    /// };
    /// let totals = usage.totals(300, 900);
    /// assert_eq!(totals.get("otter"), Some(&1_200));
    /// assert_eq!(totals.get("heron"), None);
    /// ```
    pub fn totals(&self, from: i64, to: i64) -> HashMap<&str, u64> {
        let mut totals: HashMap<&str, u64> = HashMap::new();
        for step in self
            .steps
            .iter()
            .filter(|step| (from..to).contains(&step.at.0))
        {
            *totals.entry(&step.key).or_default() += step.core_s;
        }
        totals
    }
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
    /// A column that no step covers is empty. A step that runs past [`Usage::to`] averages over the
    /// time up to it, since the server counts no further.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::UnixTime;
    /// use glurmo_model::usage::{Step, Usage};
    ///
    /// let step = |at, key: &str, core_s| Step {
    ///     at: UnixTime(at), key: key.into(), core_s, wait_s: 0,
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
    ///
    /// let so_far = Usage { to: UnixTime(450), ..usage };
    /// assert_eq!(so_far.columns(0, 600, 2)[1].cores, [("otter", 4)]);
    /// ```
    pub fn columns(&self, from: i64, span_s: i64, count: u16) -> Vec<Column<'_>> {
        let by_time = self.by_step();
        let count = i64::from(count);
        (0..count)
            .map(|i| {
                let time = from + (2 * i + 1) * span_s / (2 * count);
                by_time
                    .range(..=time)
                    .next_back()
                    .filter(|(at, _)| **at + self.step_s > time)
                    .map_or_else(Column::default, |(_, column)| column.clone())
            })
            .collect()
    }

    /// Each step that reaches into `from` up to `to`, with when it starts, earliest first. A graph that
    /// draws one bar per step at its own time keeps its shape as it is panned or zoomed, where
    /// [`Usage::columns`] samples again at every move.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::UnixTime;
    /// use glurmo_model::usage::{Step, Usage};
    ///
    /// let step = |at, key: &str, core_s| Step { at: UnixTime(at), key: key.into(), core_s, wait_s: 0 };
    /// let usage = Usage {
    ///     from: UnixTime(0),
    ///     to: UnixTime(900),
    ///     step_s: 300,
    ///     steps: vec![step(0, "heron", 300), step(300, "otter", 600), step(600, "otter", 900)],
    /// };
    /// let bars = usage.bars(400, 700);
    /// let starts: Vec<i64> = bars.iter().map(|(at, _)| at.0).collect();
    /// assert_eq!(starts, [300, 600]);
    /// assert_eq!(bars[1].1.cores, [("otter", 3)]);
    /// ```
    pub fn bars(&self, from: i64, to: i64) -> Vec<(UnixTime, Column<'_>)> {
        self.by_step()
            .into_iter()
            .filter(|(at, _)| *at + self.step_s.max(1) > from && *at < to)
            .map(|(at, column)| (UnixTime(at), column))
            .collect()
    }

    /// Every step as a column, by when it starts: mean cores per login, biggest user overall first,
    /// and mean tasks waiting. The last step may still be filling, so it averages over what it covers.
    fn by_step(&self) -> BTreeMap<i64, Column<'_>> {
        let step_s = self.step_s.max(1);
        let covered = |at: i64| u64::try_from((self.to.0 - at).clamp(1, step_s)).unwrap_or(1);
        let mut by_time: BTreeMap<i64, Column> = BTreeMap::new();
        let mut totals: HashMap<&str, u64> = HashMap::new();
        for step in &self.steps {
            let column = by_time.entry(step.at.0).or_default();
            column
                .cores
                .push((&step.key, step.core_s / covered(step.at.0)));
            column.waiting += step.wait_s;
            *totals.entry(&step.key).or_default() += step.core_s;
        }
        for (at, column) in &mut by_time {
            column.waiting /= covered(*at);
            column
                .cores
                .sort_by_cached_key(|(login, _)| (std::cmp::Reverse(totals[login]), *login));
        }
        by_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_with_equal_totals_stack_by_login() {
        let step = |key: &str| Step {
            at: UnixTime(0),
            key: key.into(),
            core_s: 600,
            wait_s: 0,
        };
        let usage = Usage {
            from: UnixTime(0),
            to: UnixTime(300),
            step_s: 300,
            steps: vec![step("otter"), step("heron"), step("lynx")],
        };
        assert_eq!(
            usage.columns(0, 300, 1)[0].cores,
            [("heron", 2), ("lynx", 2), ("otter", 2)]
        );
    }

    #[test]
    fn waiting_sums_every_login_before_it_rounds() {
        let step = |key: &str| Step {
            at: UnixTime(0),
            key: key.into(),
            core_s: 0,
            wait_s: 200,
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
    fn the_last_step_averages_over_the_time_it_covers() {
        let step = |at, core_s| Step {
            at: UnixTime(at),
            key: "otter".into(),
            core_s,
            wait_s: 150,
        };
        let usage = Usage {
            from: UnixTime(0),
            to: UnixTime(450),
            step_s: 300,
            steps: vec![step(0, 1_200), step(300, 600)],
        };
        let columns = usage.columns(0, 600, 2);
        assert_eq!(columns[0].cores, [("otter", 4)]);
        assert_eq!(columns[1].cores, [("otter", 4)]);
        assert_eq!(columns[1].waiting, 1);
    }

    #[test]
    fn a_step_travels_as_a_short_array() {
        let step = Step {
            at: UnixTime(300),
            key: "otter".into(),
            core_s: 600,
            wait_s: 30,
        };
        let json = serde_json::to_string(&step).unwrap();
        assert_eq!(json, r#"[300,"otter",600,30]"#);
        assert_eq!(serde_json::from_str::<Step>(&json).unwrap(), step);
    }

    #[test]
    fn steps_are_whole_bins_and_at_most_about_600() {
        assert_eq!(step_for(3_600), 300);
        assert_eq!(step_for(86_400), 300);
        assert_eq!(step_for(7 * 86_400), 1_200);
        assert_eq!(step_for(30 * 86_400), 4_500);
    }
}
