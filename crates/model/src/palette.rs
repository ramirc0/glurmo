//! Palettes: which colors cores take, apart from the theme that styles the rest. A palette colors by
//! user, by user group, or by how much time a job has left.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::layout::Span;
use crate::summary::{group_holdings, holdings};
use crate::theme::{Color, Theme};
use crate::{Attempt, Cluster, GroupName, Login, Occupancies, UnixTime};

/// What a palette tells apart.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::Span;
/// use glurmo_model::palette::ColorBy;
///
/// let by: ColorBy = serde_json::from_str(r#"{"TimeLeft": {"within": {"Hours": 24}}}"#).unwrap();
/// assert_eq!(by, ColorBy::TimeLeft { within: Span::Hours(24) });
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorBy {
    /// Users.
    #[default]
    User,
    /// User groups. A user takes the color of one group they are in.
    Group,
    /// How long until a job reaches its time limit.
    TimeLeft {
        /// The stretch the palette's colors split into equal steps, so a step means the same hours
        /// whatever runs. A job with more left takes the last step.
        within: Span,
    },
}

/// A palette. Names in `bound` take their colors first. Under [`ColorBy::User`] and
/// [`ColorBy::Group`], the rest then take `colors`, one each: the biggest holders first, then those who
/// only wait. Whoever is left once the colors run out takes `rest`. Under [`ColorBy::TimeLeft`],
/// `colors` are the steps, soonest first.
///
/// # Examples
///
/// ```
/// use glurmo_model::palette::{ColorBy, Palette};
///
/// let mono: Palette = serde_json::from_str(r##"{"name": "Mono", "rest": "#56b4e9"}"##).unwrap();
/// assert_eq!(mono.by, ColorBy::User);
/// assert!(mono.colors.is_empty() && mono.bound.is_empty());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Palette {
    /// The name shown in pickers.
    pub name: String,
    /// What it tells apart.
    #[serde(default)]
    pub by: ColorBy,
    /// The colors handed out in order.
    #[serde(default)]
    pub colors: Vec<Color>,
    /// Logins or group names with a color of their own. For a user in two bound groups, the first wins.
    #[serde(default)]
    pub bound: Vec<(String, Color)>,
    /// Whether `colors` stretch over every holder not bound, each taking a color mixed from the two
    /// nearest, in place of one color each for the biggest. A ramp then covers everyone.
    #[serde(default)]
    pub spread: bool,
    /// The color of everyone else, or `None` for a quiet color the view takes from its theme.
    #[serde(default)]
    pub rest: Option<Color>,
}

impl Palette {
    /// The palette a theme stands in for when none is picked: its user colors, by user.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::palette::Palette;
    /// use glurmo_model::theme::Theme;
    ///
    /// # let json = r##"{
    /// #     "name": "Rose Pine",
    /// #     "ui": {"base": "#191724", "surface": "#1f1d2e", "overlay": "#26233a",
    /// #            "muted": "#6e6a86", "subtle": "#908caa", "text": "#e0def4"},
    /// #     "state": {"free": "#26233a", "down": "#eb6f92", "drain": "#f6c177"},
    /// #     "users": ["#9ccfd8", "#c4a7e7"],
    /// #     "fonts": {"sans": "Hanken Grotesk", "mono": "CommitMono Nerd Font"}
    /// # }"##;
    /// let theme: Theme = serde_json::from_str(json).unwrap();
    /// let palette = Palette::of_theme(&theme);
    /// assert_eq!(palette.colors, theme.users);
    /// assert_eq!(palette.rest, None);
    /// ```
    pub fn of_theme(theme: &Theme) -> Self {
        Self {
            name: theme.name.clone(),
            by: ColorBy::User,
            colors: theme.users.clone(),
            bound: Vec::new(),
            spread: false,
            rest: None,
        }
    }

    /// The colors for everyone listed, in rank order, the first `holders` of whom hold cores or GPUs
    /// and the rest only wait: one each until the colors run out. Under `spread`, the whole list
    /// stretches over the holders, and those who only wait take its last color.
    fn dealt(&self, holders: usize, listed: usize) -> Vec<Color> {
        let last = self.colors.len().saturating_sub(1);
        if !self.spread || self.colors.is_empty() {
            return self.colors.iter().take(listed).cloned().collect();
        }
        (0..listed)
            .map(|rank| {
                if rank >= holders || holders < 2 || last == 0 {
                    return self.colors[if rank >= holders { last } else { 0 }].clone();
                }
                let low = rank * last / (holders - 1);
                let high = (low + 1).min(last);
                let over = rank * last - low * (holders - 1);
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "counts of users and colors are small"
                )]
                let share = over as f64 / (holders - 1) as f64;
                self.colors[low].mix(&self.colors[high], share)
            })
            .collect()
    }

    /// Who takes which color in `cluster` right now.
    ///
    /// # Examples
    ///
    /// ```
    /// # let attempt = r#"{
    /// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
    /// #     "partitions": ["5days"], "state": "pending", "state_raw": "PENDING", "cpus": 8,
    /// #     "mem_mb": null, "gpus": 0, "time_limit_s": null, "start": null, "end": null,
    /// #     "nodes": [], "reason": "Priority", "name": "align"
    /// # }"#;
    /// use glurmo_model::{Attempt, Cluster, Login};
    /// use glurmo_model::palette::Palette;
    ///
    /// let palette: Palette = serde_json::from_str(
    ///     r##"{"name": "Us", "colors": ["#111111"], "bound": [["heron", "#222222"]]}"##,
    /// ).unwrap();
    /// let mut cluster = Cluster::default();
    /// let attempt: Attempt = serde_json::from_str(attempt).unwrap();
    /// cluster.queue.insert(attempt.key, attempt);
    /// let coloring = palette.coloring(&cluster, &cluster.occupancy());
    /// assert_eq!(coloring.user(&Login("heron".into())), Some(&palette.bound[0].1));
    /// assert_eq!(coloring.user(&Login("otter".into())), Some(&palette.colors[0]), "otter only waits");
    /// assert_eq!(coloring.user(&Login("newt".into())), None, "newt has no job");
    /// ```
    pub fn coloring(&self, cluster: &Cluster, occupancy: &Occupancies) -> Coloring {
        let mut coloring = Coloring {
            by: self.by,
            steps: Vec::new(),
            users: BTreeMap::new(),
            groups: BTreeMap::new(),
            rest: self.rest.clone(),
        };
        match self.by {
            ColorBy::TimeLeft { .. } => coloring.steps.clone_from(&self.colors),
            ColorBy::User => {
                for (login, color) in &self.bound {
                    coloring.users.insert(Login(login.clone()), color.clone());
                }
                let listed: Vec<(&Login, bool)> = holdings(cluster, occupancy)
                    .users
                    .into_iter()
                    .filter(|(login, _)| !coloring.users.contains_key(*login))
                    .map(|(login, held)| (login, held.cores + held.gpus > 0))
                    .collect();
                let holders = listed.iter().filter(|(_, holds)| *holds).count();
                for ((login, _), color) in listed.iter().zip(self.dealt(holders, listed.len())) {
                    coloring.users.insert((*login).clone(), color);
                }
            }
            ColorBy::Group => {
                for (name, color) in &self.bound {
                    coloring
                        .groups
                        .insert(GroupName(name.clone()), color.clone());
                }
                let listed: Vec<(&GroupName, bool)> = group_holdings(cluster, occupancy)
                    .into_iter()
                    .filter(|group| !coloring.groups.contains_key(group.name))
                    .map(|group| (group.name, group.held.cores + group.held.gpus > 0))
                    .collect();
                let holders = listed.iter().filter(|(_, holds)| *holds).count();
                let dealt = self.dealt(holders, listed.len());
                let ranked: Vec<&GroupName> = listed
                    .iter()
                    .take(dealt.len())
                    .map(|(name, _)| *name)
                    .collect();
                for (name, color) in ranked.iter().zip(dealt) {
                    coloring.groups.insert((*name).clone(), color);
                }
                let bound = self.bound.iter().map(|(name, _)| name.as_str());
                let order: Vec<&str> = bound
                    .chain(ranked.iter().map(|name| name.0.as_str()))
                    .collect();
                for user in cluster.users.values() {
                    let first = order
                        .iter()
                        .find(|name| user.groups.iter().any(|group| group.0 == **name));
                    if let Some(color) =
                        first.and_then(|name| coloring.groups.get(&GroupName((*name).to_owned())))
                    {
                        coloring.users.insert(user.login.clone(), color.clone());
                    }
                }
            }
        }
        coloring
    }
}

/// Who takes which color: a [`Palette`] worked out for one moment of a cluster. `None` from a lookup
/// means the rest color.
///
/// # Examples
///
/// ```
/// use glurmo_model::{Cluster, GroupName};
/// use glurmo_model::palette::Palette;
///
/// let mono: Palette = serde_json::from_str(r##"{"name": "Mono", "rest": "#56b4e9"}"##).unwrap();
/// let cluster = Cluster::default();
/// let coloring = mono.coloring(&cluster, &cluster.occupancy());
/// assert_eq!(coloring.group(&GroupName("encode".into())), None);
/// assert_eq!(coloring.rest(), mono.rest.as_ref());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coloring {
    by: ColorBy,
    steps: Vec<Color>,
    users: BTreeMap<Login, Color>,
    groups: BTreeMap<GroupName, Color>,
    rest: Option<Color>,
}

impl Coloring {
    /// Whether a color says who holds a core. It does not under [`ColorBy::TimeLeft`], so tables then
    /// show no color beside a user or a group.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::Cluster;
    /// use glurmo_model::palette::Palette;
    ///
    /// let palette: Palette = serde_json::from_str(
    ///     r#"{"name": "Time left", "by": {"TimeLeft": {"within": {"Hours": 24}}}}"#,
    /// ).unwrap();
    /// let cluster = Cluster::default();
    /// assert!(!palette.coloring(&cluster, &cluster.occupancy()).names_owners());
    /// ```
    pub fn names_owners(&self) -> bool {
        !matches!(self.by, ColorBy::TimeLeft { .. })
    }

    /// The color of `login` in tables and history.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::{Cluster, Login};
    /// use glurmo_model::palette::Palette;
    ///
    /// let palette: Palette =
    ///     serde_json::from_str(r##"{"name": "Me", "bound": [["otter", "#9ccfd8"]]}"##).unwrap();
    /// let cluster = Cluster::default();
    /// let coloring = palette.coloring(&cluster, &cluster.occupancy());
    /// assert!(coloring.user(&Login("otter".into())).is_some());
    /// assert!(coloring.user(&Login("heron".into())).is_none());
    /// ```
    pub fn user(&self, login: &Login) -> Option<&Color> {
        self.users.get(login)
    }

    /// The color of a group, which only a palette by group gives.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::{Cluster, GroupName};
    /// use glurmo_model::palette::Palette;
    ///
    /// let palette: Palette = serde_json::from_str(
    ///     r##"{"name": "Us", "by": "Group", "bound": [["encode", "#9ccfd8"]]}"##,
    /// ).unwrap();
    /// let cluster = Cluster::default();
    /// let coloring = palette.coloring(&cluster, &cluster.occupancy());
    /// assert!(coloring.group(&GroupName("encode".into())).is_some());
    /// ```
    pub fn group(&self, name: &GroupName) -> Option<&Color> {
        self.groups.get(name)
    }

    /// The color of a core or GPU that `attempt` holds at `now`: its owner's, or under
    /// [`ColorBy::TimeLeft`] the step its time left falls in. A job past its limit takes the first
    /// step. One with more left than the span, or with no limit, takes the last.
    ///
    /// # Examples
    ///
    /// ```
    /// # let attempt = r#"{
    /// #     "key": {"job": 4242, "submit": 1700000000}, "array": null, "login": "otter",
    /// #     "partitions": ["5days"], "state": "running", "state_raw": "RUNNING", "cpus": 8,
    /// #     "mem_mb": null, "gpus": 0, "time_limit_s": 86400, "start": 1700000000, "end": null,
    /// #     "nodes": [], "reason": null, "name": "align"
    /// # }"#;
    /// use glurmo_model::{Attempt, Cluster, UnixTime};
    /// use glurmo_model::palette::Palette;
    ///
    /// let palette: Palette = serde_json::from_str(r##"{
    ///     "name": "Time left", "by": {"TimeLeft": {"within": {"Hours": 12}}},
    ///     "colors": ["#111111", "#222222"]
    /// }"##).unwrap();
    /// let cluster = Cluster::default();
    /// let coloring = palette.coloring(&cluster, &cluster.occupancy());
    /// let attempt: Attempt = serde_json::from_str(attempt).unwrap();
    /// let hours = |hours: i64| UnixTime(1_700_000_000 + hours * 3_600);
    /// assert_eq!(coloring.core(&attempt, hours(1)), Some(&palette.colors[1]), "23 hours are left");
    /// assert_eq!(coloring.core(&attempt, hours(13)), Some(&palette.colors[1]));
    /// assert_eq!(coloring.core(&attempt, hours(20)), Some(&palette.colors[0]));
    /// assert_eq!(coloring.core(&attempt, hours(30)), Some(&palette.colors[0]));
    /// ```
    pub fn core(&self, attempt: &Attempt, now: UnixTime) -> Option<&Color> {
        let ColorBy::TimeLeft { within } = self.by else {
            return self.user(&attempt.login);
        };
        let left = attempt
            .start
            .zip(attempt.time_limit_s)
            .and_then(|(start, limit)| {
                Some((start.0.checked_add(i64::try_from(limit).ok()?)? - now.0).max(0))
            });
        let step = left.and_then(|left| {
            let steps = i64::try_from(self.steps.len()).ok()?;
            usize::try_from(left.checked_mul(steps)?.checked_div(within.seconds())?).ok()
        });
        let last = self.steps.len().checked_sub(1)?;
        self.steps.get(step.map_or(last, |step| step.min(last)))
    }

    /// The color of everyone and everything no lookup colors, or `None` for the view's quiet color.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::Cluster;
    /// use glurmo_model::palette::Palette;
    ///
    /// let palette: Palette = serde_json::from_str(r#"{"name": "Plain"}"#).unwrap();
    /// let cluster = Cluster::default();
    /// assert_eq!(palette.coloring(&cluster, &cluster.occupancy()).rest(), None);
    /// ```
    pub fn rest(&self) -> Option<&Color> {
        self.rest.as_ref()
    }

    /// Under [`ColorBy::TimeLeft`], each step's color with the seconds left at which it ends, soonest
    /// first. The last step has no end in effect, since it also takes every job with more left. Empty
    /// otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::Cluster;
    /// use glurmo_model::palette::Palette;
    ///
    /// let palette: Palette = serde_json::from_str(r##"{
    ///     "name": "Time left", "by": {"TimeLeft": {"within": {"Hours": 12}}},
    ///     "colors": ["#111111", "#222222"]
    /// }"##).unwrap();
    /// let cluster = Cluster::default();
    /// let coloring = palette.coloring(&cluster, &cluster.occupancy());
    /// let ends: Vec<i64> = coloring.steps().map(|(end_s, _)| end_s).collect();
    /// assert_eq!(ends, [21_600, 43_200]);
    /// ```
    pub fn steps(&self) -> impl Iterator<Item = (i64, &Color)> {
        let within_s = match self.by {
            ColorBy::TimeLeft { within } => within.seconds(),
            ColorBy::User | ColorBy::Group => 0,
        };
        let count = i64::try_from(self.steps.len()).unwrap_or(i64::MAX).max(1);
        (1..)
            .zip(&self.steps)
            .map(move |(step, color)| (within_s * step / count, color))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Delta, Event, User};

    fn color(hex: &str) -> Color {
        Color::try_from(hex.to_owned()).unwrap()
    }

    /// A cluster that has seen `users`, each with their groups.
    fn cluster(users: &[(&str, &[&str])]) -> Cluster {
        let mut cluster = Cluster::default();
        let users = users
            .iter()
            .map(|(login, groups)| User {
                login: Login((*login).to_owned()),
                name: None,
                color_slot: 0,
                groups: groups
                    .iter()
                    .map(|group| GroupName((*group).to_owned()))
                    .collect(),
            })
            .collect();
        cluster.apply(&Delta {
            seq: 1,
            at: UnixTime(0),
            events: vec![Event::Users(users)],
        });
        cluster
    }

    #[test]
    fn spread_colors_cover_every_holder_and_plain_ones_run_out() {
        let mut ramp = Palette {
            name: "ramp".into(),
            by: ColorBy::User,
            colors: vec![color("#000000"), color("#808080"), color("#ffffff")],
            bound: Vec::new(),
            spread: true,
            rest: None,
        };
        let hex =
            |colors: Vec<Color>| -> Vec<String> { colors.into_iter().map(String::from).collect() };
        assert_eq!(
            hex(ramp.dealt(5, 5)),
            ["#000000", "#404040", "#808080", "#c0c0c0", "#ffffff"]
        );
        assert_eq!(
            hex(ramp.dealt(2, 4)),
            ["#000000", "#ffffff", "#ffffff", "#ffffff"]
        );
        assert_eq!(hex(ramp.dealt(1, 2)), ["#000000", "#ffffff"]);
        assert_eq!(ramp.dealt(0, 0), []);
        ramp.spread = false;
        assert_eq!(hex(ramp.dealt(2, 5)), ["#000000", "#808080", "#ffffff"]);
    }

    #[test]
    fn a_user_in_two_bound_groups_takes_the_first() {
        let palette = Palette {
            name: "labs".into(),
            by: ColorBy::Group,
            spread: false,
            colors: Vec::new(),
            bound: vec![
                ("mohd".into(), color("#111111")),
                ("encode".into(), color("#222222")),
            ],
            rest: None,
        };
        let cluster = cluster(&[
            ("otter", &["encode", "mohd"]),
            ("heron", &["encode"]),
            ("newt", &[]),
        ]);
        let coloring = palette.coloring(&cluster, &cluster.occupancy());
        assert_eq!(
            coloring.user(&Login("otter".into())),
            Some(&color("#111111"))
        );
        assert_eq!(
            coloring.user(&Login("heron".into())),
            Some(&color("#222222"))
        );
        assert_eq!(coloring.user(&Login("newt".into())), None);
    }
}
