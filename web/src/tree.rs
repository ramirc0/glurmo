//! Edits to a layout's split tree, for the panel builder.

use rtviz_model::layout::{Direction, Pane, Size};

/// Where a pane sits in a tree: the index of each split's child on the way down from the root.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Path(Vec<usize>);

impl Path {
    /// The root pane.
    pub fn root() -> Self {
        Self::default()
    }

    /// The `index`th child of the split here.
    pub fn child(&self, index: usize) -> Self {
        let mut steps = self.0.clone();
        steps.push(index);
        Self(steps)
    }

    /// The split this pane sits in and its index there, or `None` for the root.
    pub fn parent(&self) -> Option<(Self, usize)> {
        let (&last, rest) = self.0.split_last()?;
        Some((Self(rest.to_vec()), last))
    }

    /// Every pane from the root down to this one, this one last.
    pub fn lineage(&self) -> Vec<Self> {
        (0..=self.0.len())
            .map(|depth| Self(self.0[..depth].to_vec()))
            .collect()
    }

    /// Whether `other` is this pane or inside it.
    pub fn holds(&self, other: &Self) -> bool {
        other.0.starts_with(&self.0)
    }
}

/// The pane at `path`.
pub fn at<'a>(root: &'a Pane, path: &Path) -> Option<&'a Pane> {
    path.0.iter().try_fold(root, |pane, &index| match pane {
        Pane::Split(_, children) => children.get(index).map(|(_, child)| child),
        _ => None,
    })
}

/// The pane at `path`, to change.
pub fn at_mut<'a>(root: &'a mut Pane, path: &Path) -> Option<&'a mut Pane> {
    path.0.iter().try_fold(root, |pane, &index| match pane {
        Pane::Split(_, children) => children.get_mut(index).map(|(_, child)| child),
        _ => None,
    })
}

/// The size of the pane at `path` in its split. The root has none.
pub fn size(root: &Pane, path: &Path) -> Option<Size> {
    let (parent, index) = path.parent()?;
    match at(root, &parent)? {
        Pane::Split(_, children) => children.get(index).map(|(size, _)| *size),
        _ => None,
    }
}

/// The size of the pane at `path` in its split, to change. The root has none.
pub fn size_mut<'a>(root: &'a mut Pane, path: &Path) -> Option<&'a mut Size> {
    let (parent, index) = path.parent()?;
    match at_mut(root, &parent)? {
        Pane::Split(_, children) => children.get_mut(index).map(|(size, _)| size),
        _ => None,
    }
}

/// Puts a copy of the pane at `path` beside it, running `direction`, and returns the copy's path. A
/// split already running that way gains a pane; otherwise the pane becomes a split of two.
pub fn split(root: &mut Pane, path: &Path, direction: Direction) -> Option<Path> {
    if let Some((parent, index)) = path.parent()
        && let Some(Pane::Split(running, children)) = at_mut(root, &parent)
        && *running == direction
    {
        let copy = children.get(index)?.1.clone();
        children.insert(index + 1, (Size::Fill(1), copy));
        return Some(parent.child(index + 1));
    }
    let pane = at_mut(root, path)?;
    let copy = pane.clone();
    *pane = Pane::Split(
        direction,
        vec![(Size::Fill(1), copy.clone()), (Size::Fill(1), copy)],
    );
    Some(path.child(1))
}

/// Takes out the pane at `path` and returns the path of the split it was in. A split left with one
/// pane gives way to that pane. The root stays.
pub fn remove(root: &mut Pane, path: &Path) -> Option<Path> {
    let (parent, index) = path.parent()?;
    let split = at_mut(root, &parent)?;
    let Pane::Split(_, children) = split else {
        return None;
    };
    if index >= children.len() {
        return None;
    }
    children.remove(index);
    if children.len() == 1 {
        let (_, only) = children.remove(0);
        *split = only;
    }
    Some(parent)
}

/// Swaps the panes at `a` and `b`, each keeping the size of the slot it moves into. Nothing happens
/// when one holds the other.
pub fn swap(root: &mut Pane, a: &Path, b: &Path) -> bool {
    if a.holds(b) || b.holds(a) {
        return false;
    }
    let (Some(first), Some(second)) = (at(root, a).cloned(), at(root, b).cloned()) else {
        return false;
    };
    if let Some(pane) = at_mut(root, a) {
        *pane = second;
    }
    if let Some(pane) = at_mut(root, b) {
        *pane = first;
    }
    true
}

/// A split's sizes after the gap after pane `gap` was dragged, given each pane's length in pixels
/// before and after.
///
/// A length or percentage beside the gap scales with its pane. When both panes beside the gap fill,
/// every fill in the split is weighted by its share of the filled space, in percent. A minimum stays,
/// because its pane's length does not say what the minimum should be.
pub fn dragged(sizes: &[Size], gap: usize, before: &[f64], after: &[f64]) -> Vec<Size> {
    let beside = |index: usize| index == gap || index == gap + 1;
    let both_fill = matches!(
        (sizes.get(gap), sizes.get(gap + 1)),
        (Some(Size::Fill(_)), Some(Size::Fill(_)))
    );
    let filled: f64 = sizes
        .iter()
        .zip(after)
        .filter(|(size, _)| matches!(size, Size::Fill(_)))
        .map(|(_, length)| length)
        .sum();
    let scaled = |value: u16, index: usize| {
        let (from, to) = (before[index], after[index]);
        if from <= 0.0 {
            return value;
        }
        whole(f64::from(value) * to / from)
    };
    sizes
        .iter()
        .enumerate()
        .map(|(index, &size)| match size {
            Size::Length(lines) if beside(index) => Size::Length(scaled(lines, index)),
            Size::Percentage(share) if beside(index) => {
                Size::Percentage(scaled(share, index).min(100))
            }
            Size::Fill(_) if both_fill && filled > 0.0 => {
                Size::Fill(whole(100.0 * after[index] / filled))
            }
            other => other,
        })
        .collect()
}

/// `value` rounded to a whole number, at least 1.
fn whole(value: f64) -> u16 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to u16"
    )]
    let whole = value.round().clamp(1.0, f64::from(u16::MAX)) as u16;
    whole
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtviz_model::layout::Grouping;

    /// `Header` over `Die | (Users / Ticker)`, sized as the built-in layouts are.
    fn tree() -> Pane {
        Pane::Split(
            Direction::Vertical,
            vec![
                (Size::Length(2), Pane::Header),
                (
                    Size::Fill(1),
                    Pane::Split(
                        Direction::Horizontal,
                        vec![
                            (
                                Size::Percentage(70),
                                Pane::Die {
                                    group: Grouping::Hardware,
                                },
                            ),
                            (
                                Size::Fill(1),
                                Pane::Split(
                                    Direction::Vertical,
                                    vec![
                                        (Size::Length(12), Pane::Users),
                                        (Size::Fill(1), Pane::Ticker),
                                    ],
                                ),
                            ),
                        ],
                    ),
                ),
            ],
        )
    }

    fn path(steps: &[usize]) -> Path {
        Path(steps.to_vec())
    }

    #[test]
    fn paths_find_panes_and_their_sizes() {
        let mut root = tree();
        assert_eq!(at(&root, &path(&[1, 1, 1])), Some(&Pane::Ticker));
        assert_eq!(at(&root, &path(&[0, 0])), None, "a panel has no children");
        assert_eq!(at(&root, &path(&[5])), None);
        assert_eq!(
            size_mut(&mut root, &path(&[1, 0])),
            Some(&mut Size::Percentage(70))
        );
        assert_eq!(size_mut(&mut root, &Path::root()), None);
        assert_eq!(size(&root, &path(&[1, 1, 0])), Some(Size::Length(12)));
        assert_eq!(size(&root, &Path::root()), None);
        assert_eq!(path(&[1, 1]).parent(), Some((path(&[1]), 1)));
        assert_eq!(
            path(&[1, 1]).lineage(),
            [Path::root(), path(&[1]), path(&[1, 1])]
        );
    }

    #[test]
    fn splitting_the_way_the_parent_runs_adds_a_sibling() {
        let mut root = tree();
        let copy = split(&mut root, &path(&[1, 1, 0]), Direction::Vertical).unwrap();
        assert_eq!(copy, path(&[1, 1, 1]));
        assert_eq!(at(&root, &copy), Some(&Pane::Users));
        assert_eq!(at(&root, &path(&[1, 1, 2])), Some(&Pane::Ticker));
        assert_eq!(size_mut(&mut root, &copy), Some(&mut Size::Fill(1)));
    }

    #[test]
    fn splitting_across_the_parent_nests_a_split_in_the_same_slot() {
        let mut root = tree();
        let copy = split(&mut root, &path(&[0]), Direction::Horizontal).unwrap();
        assert_eq!(copy, path(&[0, 1]));
        assert_eq!(size_mut(&mut root, &path(&[0])), Some(&mut Size::Length(2)));
        assert_eq!(at(&root, &path(&[0, 0])), Some(&Pane::Header));
        assert_eq!(at(&root, &copy), Some(&Pane::Header));
    }

    #[test]
    fn the_root_panel_splits_too() {
        let mut root = Pane::Users;
        let copy = split(&mut root, &Path::root(), Direction::Vertical).unwrap();
        assert_eq!(copy, path(&[1]));
        assert!(matches!(&root, Pane::Split(Direction::Vertical, children) if children.len() == 2));
    }

    #[test]
    fn removing_one_of_two_leaves_the_other_in_the_parents_slot() {
        let mut root = tree();
        assert_eq!(remove(&mut root, &path(&[1, 1, 0])), Some(path(&[1, 1])));
        assert_eq!(at(&root, &path(&[1, 1])), Some(&Pane::Ticker));
        assert_eq!(
            size_mut(&mut root, &path(&[1, 1])),
            Some(&mut Size::Fill(1))
        );
        assert_eq!(remove(&mut root, &Path::root()), None);
    }

    #[test]
    fn removing_one_of_three_keeps_the_split() {
        let mut root = tree();
        split(&mut root, &path(&[1, 1, 0]), Direction::Vertical);
        remove(&mut root, &path(&[1, 1, 0]));
        assert_eq!(at(&root, &path(&[1, 1, 0])), Some(&Pane::Users));
        assert_eq!(at(&root, &path(&[1, 1, 1])), Some(&Pane::Ticker));
    }

    #[test]
    fn swapping_moves_panes_between_slots_but_not_into_themselves() {
        let mut root = tree();
        assert!(swap(&mut root, &path(&[0]), &path(&[1, 1, 1])));
        assert_eq!(at(&root, &path(&[0])), Some(&Pane::Ticker));
        assert_eq!(at(&root, &path(&[1, 1, 1])), Some(&Pane::Header));
        assert_eq!(size_mut(&mut root, &path(&[0])), Some(&mut Size::Length(2)));
        let before = root.clone();
        assert!(!swap(&mut root, &path(&[1]), &path(&[1, 0])));
        assert_eq!(root, before);
    }

    #[test]
    fn dragging_scales_lengths_and_percentages_and_leaves_one_fill_alone() {
        let sizes = [Size::Length(2), Size::Fill(1)];
        assert_eq!(
            dragged(&sizes, 0, &[40.0, 1000.0], &[61.0, 979.0]),
            [Size::Length(3), Size::Fill(1)]
        );
        let sizes = [Size::Percentage(70), Size::Fill(1)];
        assert_eq!(
            dragged(&sizes, 0, &[700.0, 300.0], &[600.0, 400.0]),
            [Size::Percentage(60), Size::Fill(1)]
        );
        let sizes = [Size::Min(10), Size::Length(4)];
        assert_eq!(
            dragged(&sizes, 0, &[300.0, 80.0], &[340.0, 40.0]),
            [Size::Min(10), Size::Length(2)]
        );
    }

    #[test]
    fn dragging_between_two_fills_weighs_every_fill_by_its_share() {
        let sizes = [
            Size::Length(12),
            Size::Fill(1),
            Size::Fill(1),
            Size::Fill(2),
        ];
        let after = [240.0, 150.0, 50.0, 200.0];
        assert_eq!(
            dragged(&sizes, 1, &[240.0, 100.0, 100.0, 200.0], &after),
            [
                Size::Length(12),
                Size::Fill(38),
                Size::Fill(13),
                Size::Fill(50)
            ]
        );
    }

    #[test]
    fn lengths_never_drop_to_zero() {
        assert_eq!(
            dragged(
                &[Size::Length(2), Size::Fill(1)],
                0,
                &[40.0, 600.0],
                &[2.0, 638.0]
            ),
            [Size::Length(1), Size::Fill(1)]
        );
    }
}
