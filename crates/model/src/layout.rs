//! Layouts: split trees of panels, in the style of rmpc.

use serde::{Deserialize, Serialize};

/// A named arrangement of panels.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::{Layout, Pane};
///
/// let layout = Layout { name: "bare".into(), root: Pane::Users };
/// assert_eq!(layout.root, Pane::Users);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// The name shown in pickers.
    pub name: String,
    /// The outermost pane.
    pub root: Pane,
}

/// A split into sized panes, or one panel.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::{Direction, Pane, Size};
///
/// let pane = Pane::Split(Direction::Vertical, vec![(Size::Length(1), Pane::Header), (Size::Fill(1), Pane::Users)]);
/// assert!(matches!(pane, Pane::Split(Direction::Vertical, _)));
///
/// let old: Pane = serde_json::from_str(r#"{"Die": {"group": "Hardware"}}"#).unwrap();
/// assert!(matches!(old, Pane::Tiles { .. }), "the name from before the rename still reads");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    /// Panes side by side or stacked, each with its size.
    Split(Direction, Vec<(Size, Pane)>),
    /// The cluster name, the time and the totals.
    Header,
    /// The key bindings.
    Keys,
    /// Every core and GPU, a tile per node, colored by the palette. Layouts from before the rename
    /// call it `Die`.
    #[serde(alias = "Die")]
    Tiles {
        /// How to group nodes.
        group: Grouping,
    },
    /// Who holds how many cores and GPUs.
    Users,
    /// What each user group's members hold.
    Groups,
    /// Pending jobs.
    Queue {
        /// How to sum them.
        by: QueueBy,
    },
    /// Each partition's load and limits.
    Partitions,
    /// Node health.
    Nodes,
    /// Cores in use and queue depth over time.
    History {
        /// How far back to show.
        span: Span,
        /// The order its users stack in.
        #[serde(default)]
        stack: StackBy,
    },
    /// Your own jobs.
    Mine,
    /// Running and pending jobs, grouped, to search and filter.
    Jobs,
    /// Starts and ends as they happen.
    Ticker,
}

/// Which way a split runs.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::Direction;
///
/// assert_ne!(Direction::Vertical, Direction::Horizontal);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Panes stacked top to bottom.
    Vertical,
    /// Panes side by side.
    Horizontal,
}

/// A pane's share of its split. Each maps onto the ratatui constraint of the same name.
/// On the web, a length counts lines of monospace text.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::Size;
///
/// let size: Size = serde_json::from_str(r#"{"Percentage": 66}"#).unwrap();
/// assert_eq!(size, Size::Percentage(66));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Size {
    /// Exactly this many lines or columns.
    Length(u16),
    /// This percentage of the split.
    Percentage(u16),
    /// A share of the space left over, by weight.
    Fill(u16),
    /// At least this many lines or columns.
    Min(u16),
}

/// How a panel groups nodes or jobs.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::Grouping;
///
/// assert_eq!(serde_json::to_string(&Grouping::Hardware).unwrap(), r#""Hardware""#);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grouping {
    /// By hardware: CPU generation, core count and GPU model.
    Hardware,
    /// By partition.
    Partition,
}

/// How the queue panel sums pending jobs.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::QueueBy;
///
/// assert_eq!(serde_json::to_string(&QueueBy::Login).unwrap(), r#""Login""#);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueBy {
    /// One row per partition, with the most common reason to wait.
    Partition,
    /// One row per user.
    Login,
}

/// The order a history panel stacks its users in, from the bottom up.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::StackBy;
///
/// assert_eq!(StackBy::default(), StackBy::FirstSeen);
/// assert!(StackBy::Biggest.place(Some(3), 900) < StackBy::Biggest.place(Some(0), 100));
/// assert!(StackBy::FirstSeen.place(Some(0), 100) < StackBy::FirstSeen.place(Some(3), 900));
/// assert_eq!(StackBy::Name.place(Some(3), 900), StackBy::Name.place(None, 0));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StackBy {
    /// In the order glurmo first saw each user, which never changes.
    #[default]
    FirstSeen,
    /// Whoever held the most over the stretch shown, at the bottom.
    Biggest,
    /// By login.
    Name,
}

impl StackBy {
    /// Where a user stacks, lowest at the bottom, given the slot they were first seen in and the
    /// core-seconds they held over the stretch shown. Users with one place stack by login.
    #[must_use]
    pub fn place(self, seen: Option<u32>, held_core_s: u64) -> u64 {
        match self {
            Self::FirstSeen => seen.map_or(u64::MAX, u64::from),
            Self::Biggest => u64::MAX - held_core_s,
            Self::Name => 0,
        }
    }
}

/// A stretch of time back from now.
///
/// # Examples
///
/// ```
/// use glurmo_model::layout::Span;
///
/// assert_eq!(serde_json::to_string(&Span::Hours(24)).unwrap(), r#"{"Hours":24}"#);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Span {
    /// Hours.
    Hours(u32),
    /// Days.
    Days(u32),
}

impl Span {
    /// The span in seconds.
    ///
    /// # Examples
    ///
    /// ```
    /// use glurmo_model::layout::Span;
    ///
    /// assert_eq!(Span::Hours(2).seconds(), 7_200);
    /// assert_eq!(Span::Days(1).seconds(), 86_400);
    /// ```
    pub fn seconds(self) -> i64 {
        match self {
            Self::Hours(hours) => i64::from(hours) * 3_600,
            Self::Days(days) => i64::from(days) * 86_400,
        }
    }
}
