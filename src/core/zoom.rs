//! Icon size levels of the three views (the zoom): the grid has four, the list
//! and the compact view three, and Medium is today's size in both. This module is the pure part:
//! the levels, stepping, and what a zoom command does to one. The pixel
//! sizes of each level live with the views that draw them.

use serde::{Deserialize, Serialize};

/// A zoom level: an ordered, non-empty list of levels with one default.
pub trait Level: Copy + PartialEq + 'static {
    /// Every level, smallest first.
    const LEVELS: &'static [Self];
    /// The level that draws today's sizes.
    const MEDIUM: Self;

    /// The next larger level; the largest stays.
    #[must_use]
    fn zoomed_in(self) -> Self {
        let at = Self::LEVELS.iter().position(|level| *level == self);
        at.and_then(|at| Self::LEVELS.get(at + 1))
            .copied()
            .unwrap_or(self)
    }

    /// The next smaller level; the smallest stays.
    #[must_use]
    fn zoomed_out(self) -> Self {
        let at = Self::LEVELS.iter().position(|level| *level == self);
        at.and_then(|at| at.checked_sub(1))
            .and_then(|at| Self::LEVELS.get(at))
            .copied()
            .unwrap_or(self)
    }
}

/// The icon size of the grid view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridZoom {
    Small,
    #[default]
    Medium,
    Large,
    Larger,
}

impl GridZoom {
    /// Every level, smallest first.
    pub const ALL: [Self; 4] = [Self::Small, Self::Medium, Self::Large, Self::Larger];
}

impl Level for GridZoom {
    const LEVELS: &'static [Self] = &Self::ALL;
    const MEDIUM: Self = Self::Medium;
}

/// The row and icon size of the list view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListZoom {
    Small,
    #[default]
    Medium,
    Large,
}

impl ListZoom {
    /// Every level, smallest first.
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];
}

impl Level for ListZoom {
    const LEVELS: &'static [Self] = &Self::ALL;
    const MEDIUM: Self = Self::Medium;
}

/// The cell and icon size of the compact view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactZoom {
    Small,
    #[default]
    Medium,
    Large,
}

impl CompactZoom {
    /// Every level, smallest first.
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];
}

impl Level for CompactZoom {
    const LEVELS: &'static [Self] = &Self::ALL;
    const MEDIUM: Self = Self::Medium;
}

/// What a zoom control asks for: one step in, one step out, or back to
/// Medium.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoomCommand {
    In,
    Out,
    Reset,
}

impl ZoomCommand {
    /// `level` after the command.
    #[must_use]
    pub fn apply<L: Level>(self, level: L) -> L {
        match self {
            Self::In => level.zoomed_in(),
            Self::Out => level.zoomed_out(),
            Self::Reset => L::MEDIUM,
        }
    }

    /// Whether the command moves `level` at all.
    #[must_use]
    pub fn changes<L: Level>(self, level: L) -> bool {
        self.apply(level) != level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_stepping_clamps_at_both_ends_and_reset_returns_to_medium() {
        use GridZoom::{Large, Larger, Medium, Small};
        assert_eq!(Small.zoomed_out(), Small, "clamped at the smallest");
        assert_eq!(Small.zoomed_in(), Medium);
        assert_eq!(Medium.zoomed_in(), Large);
        assert_eq!(Large.zoomed_in(), Larger);
        assert_eq!(Larger.zoomed_in(), Larger, "clamped at the largest");
        assert_eq!(Larger.zoomed_out(), Large);
        assert_eq!(Medium.zoomed_out(), Small);
        assert_eq!(GridZoom::default(), Medium);
        for level in GridZoom::ALL {
            assert_eq!(ZoomCommand::Reset.apply(level), Medium);
        }
    }

    #[test]
    fn list_stepping_has_three_levels_clamped_at_both_ends() {
        use ListZoom::{Large, Medium, Small};
        assert_eq!(ListZoom::ALL, [Small, Medium, Large]);
        assert_eq!(Small.zoomed_out(), Small);
        assert_eq!(Small.zoomed_in(), Medium);
        assert_eq!(Medium.zoomed_in(), Large);
        assert_eq!(Large.zoomed_in(), Large);
        assert_eq!(Large.zoomed_out(), Medium);
        assert_eq!(ListZoom::default(), Medium);
        assert_eq!(ZoomCommand::Reset.apply(Large), Medium);
    }

    #[test]
    fn compact_stepping_has_three_levels_and_defaults_to_medium() {
        use CompactZoom::{Large, Medium, Small};
        assert_eq!(CompactZoom::ALL, [Small, Medium, Large]);
        assert_eq!(CompactZoom::default(), Medium);
        assert_eq!(Small.zoomed_out(), Small);
        assert_eq!(Small.zoomed_in(), Medium);
        assert_eq!(Large.zoomed_in(), Large);
        assert_eq!(Large.zoomed_out(), Medium);
        assert_eq!(ZoomCommand::Reset.apply(Small), Medium);
    }

    #[test]
    fn commands_step_one_level_and_can_report_whether_anything_changed() {
        assert_eq!(ZoomCommand::In.apply(GridZoom::Medium), GridZoom::Large);
        assert_eq!(ZoomCommand::Out.apply(GridZoom::Medium), GridZoom::Small);
        assert_eq!(ZoomCommand::In.apply(ListZoom::Large), ListZoom::Large);
        assert!(ZoomCommand::In.changes(GridZoom::Medium));
        assert!(!ZoomCommand::In.changes(GridZoom::Larger));
        assert!(!ZoomCommand::Out.changes(ListZoom::Small));
        assert!(!ZoomCommand::Reset.changes(ListZoom::Medium));
        assert!(ZoomCommand::Reset.changes(ListZoom::Small));
    }

    #[test]
    fn levels_serialize_as_snake_case_names() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Holder {
            grid: GridZoom,
            list: ListZoom,
        }
        let text = toml::to_string(&Holder {
            grid: GridZoom::Larger,
            list: ListZoom::Small,
        });
        assert_eq!(text.as_deref(), Ok("grid = \"larger\"\nlist = \"small\"\n"));
        assert!(toml::from_str::<Holder>("grid = \"huge\"\nlist = \"small\"\n").is_err());
        assert!(toml::from_str::<Holder>("grid = \"larger\"\nlist = \"larger\"\n").is_err());
    }
}
