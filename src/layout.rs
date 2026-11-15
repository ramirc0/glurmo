//! Layouts: split trees of panels, in the style of rmpc.

use serde::{Deserialize, Serialize};

/// A named arrangement of panels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// The name shown in pickers.
    pub name: String,
    /// The outermost pane.
    pub root: Pane,
}

/// A split into sized panes, or one panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pane {
    /// Panes side by side or stacked, each with its size.
    Split(Direction, Vec<(Size, Pane)>),
    /// The cluster name, the time and the totals.
    Header,
    /// The key bindings.
    Keys,
    /// Every core and GPU, colored by who holds it.
    Die {
        /// How to group nodes.
        group: Grouping,
    },
    /// Who holds how many cores and GPUs.
    Users,
    /// Pending jobs.
    Queue {
        /// How to group them.
        group: Grouping,
    },
    /// Each partition's load and limits.
    Partitions,
    /// Node health.
    Nodes,
    /// Cores in use and queue depth over time.
    History {
        /// How far back to show.
        span: Span,
    },
    /// Your own jobs.
    Mine,
    /// Starts and ends as they happen.
    Ticker,
}

/// Which way a split runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Panes stacked top to bottom.
    Vertical,
    /// Panes side by side.
    Horizontal,
}

/// A pane's share of its split. Each maps onto the ratatui constraint of the same name.
/// On the web, a length counts lines of monospace text.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grouping {
    /// By hardware: CPU generation, core count and GPU model.
    Hardware,
    /// By partition.
    Partition,
}

/// A stretch of time back from now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Span {
    /// Hours.
    Hours(u32),
    /// Days.
    Days(u32),
}
