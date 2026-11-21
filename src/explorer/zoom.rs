//! What each zoom level draws: the grid tile and its art, the list row and
//! its icon, the thumbnail bucket a tile asks for, and how the mouse wheel
//! turns into a zoom step. The levels and their stepping are
//! [`fileflux::core::zoom`]; Medium is exactly the size the views had before
//! the zoom existed.

use fileflux::core::thumbnails::Bucket;
use fileflux::core::zoom::{CompactZoom, GridZoom, ListZoom, ZoomCommand};
use gpui_component::Size;

use super::state::View;

/// Gap between grid tiles, at every level.
pub(crate) const TILE_GAP: f32 = 6.0;
/// Tile top padding above the thumbnail slot.
pub(crate) const TILE_TOP_PADDING: f32 = 12.0;
/// Gap between the thumbnail slot and the filename.
pub(crate) const NAME_GAP: f32 = 8.0;
/// One filename line's height; long names clip at two lines.
pub(crate) const NAME_LINE: f32 = 17.0;
/// Gap between the filename and the size subline.
pub(crate) const SUB_GAP: f32 = 2.0;
/// The mono 11 px subline's height.
pub(crate) const SUB_LINE: f32 = 14.0;
/// Tile bottom padding under the subline.
pub(crate) const TILE_BOTTOM_PADDING: f32 = 4.0;

/// The sizes of one grid level, in logical pixels. The text, the paddings
/// and the gaps do not scale: only the tile and its art do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridMetrics {
    /// Every tile draws at exactly this width, so a partial last row matches
    /// a full one.
    pub(crate) tile_width: f32,
    /// The slot every glyph, folder and photo draws in.
    pub(crate) thumb_width: f32,
    pub(crate) thumb_height: f32,
    /// A file glyph inside the slot.
    pub(crate) glyph: f32,
    /// The bordered photo box a real thumbnail fills.
    pub(crate) photo_width: f32,
    pub(crate) photo_height: f32,
    /// The icon inside the photo box while no thumbnail shows.
    pub(crate) photo_glyph: f32,
}

impl GridMetrics {
    /// Tile height: top padding, the slot, the name gap, two clamped
    /// filename lines, the subline gap and subline, and bottom padding.
    pub(crate) fn tile_height(&self) -> f32 {
        TILE_TOP_PADDING
            + self.thumb_height
            + NAME_GAP
            + 2.0 * NAME_LINE
            + SUB_GAP
            + SUB_LINE
            + TILE_BOTTOM_PADDING
    }
}

/// The grid size table.
pub(crate) trait GridZoomExt {
    fn metrics(self) -> GridMetrics;
    /// The freedesktop size a photo box of this level needs.
    fn thumbnail_bucket(self) -> Bucket;
}

impl GridZoomExt for GridZoom {
    fn metrics(self) -> GridMetrics {
        let (tile_width, thumb, glyph, photo, photo_glyph) = match self {
            Self::Small => (88.0, (48.0, 40.0), 30.0, (46.0, 34.0), 22.0),
            // Today's tile.
            Self::Medium => (112.0, (64.0, 52.0), 40.0, (62.0, 46.0), 28.0),
            Self::Large => (160.0, (112.0, 88.0), 64.0, (110.0, 82.0), 44.0),
            Self::Larger => (224.0, (176.0, 136.0), 96.0, (174.0, 130.0), 64.0),
        };
        GridMetrics {
            tile_width,
            thumb_width: thumb.0,
            thumb_height: thumb.1,
            glyph,
            photo_width: photo.0,
            photo_height: photo.1,
            photo_glyph,
        }
    }

    fn thumbnail_bucket(self) -> Bucket {
        let photo = self.metrics();
        Bucket::for_display(photo.photo_width.max(photo.photo_height))
    }
}

/// Derives the grid column count from the entries area's **inner** width —
/// the content box the tiles lay out in; the padding is already excluded by
/// the measurement — with a minimum of one so nothing is ever unreachable.
pub(crate) fn grid_columns(inner_width: f32, zoom: GridZoom) -> usize {
    let tile = zoom.metrics().tile_width;
    let usable = inner_width.max(tile);
    (((usable + TILE_GAP) / (tile + TILE_GAP)).floor() as usize).max(1)
}

/// The sizes of one compact level, in logical pixels: a fixed-width cell
/// holding the icon and a single-line name. Cells sit [`TILE_GAP`] apart
/// across and touch vertically, like the grid's rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CompactMetrics {
    pub(crate) cell_width: f32,
    pub(crate) cell_height: f32,
    pub(crate) icon: f32,
}

/// The compact size table.
pub(crate) trait CompactZoomExt {
    fn metrics(self) -> CompactMetrics;
}

impl CompactZoomExt for CompactZoom {
    fn metrics(self) -> CompactMetrics {
        let (cell_width, cell_height, icon) = match self {
            Self::Small => (150.0, 28.0, 16.0),
            Self::Medium => (190.0, 32.0, 20.0),
            Self::Large => (240.0, 36.0, 24.0),
        };
        CompactMetrics {
            cell_width,
            cell_height,
            icon,
        }
    }
}

/// The compact column count for the entries area's inner width, never zero.
pub(crate) fn compact_columns(inner_width: f32, zoom: CompactZoom) -> usize {
    let cell = zoom.metrics().cell_width;
    let usable = inner_width.max(cell);
    (((usable + TILE_GAP) / (cell + TILE_GAP)).floor() as usize).max(1)
}

/// What Up and Down move by: one entry in the list, one row of cells in the
/// grid and the compact view.
pub(crate) fn vertical_step(view: View, columns: usize) -> isize {
    match view {
        View::List => 1,
        View::Grid | View::Compact => columns.max(1) as isize,
    }
}

/// The list size table: the table's own size steps give the row height.
pub(crate) trait ListZoomExt {
    /// The table size whose row height this level draws; Medium is the
    /// `Small` the list always used.
    fn table_size(self) -> Size;
    fn row_height(self) -> f32;
    /// The icon in the name cell.
    fn icon(self) -> f32;
}

impl ListZoomExt for ListZoom {
    fn table_size(self) -> Size {
        match self {
            Self::Small => Size::XSmall,
            Self::Medium => Size::Small,
            Self::Large => Size::Large,
        }
    }

    fn row_height(self) -> f32 {
        f32::from(self.table_size().table_row_height())
    }

    fn icon(self) -> f32 {
        match self {
            Self::Small => 12.0,
            Self::Medium => 14.0,
            Self::Large => 20.0,
        }
    }
}

/// How far the wheel turns, in logical pixels, for one zoom step: one line.
pub(crate) const WHEEL_NOTCH: f32 = 40.0;

/// The wheel movement not yet spent on a zoom step, so a touchpad's many
/// small deltas and a mouse's single notch both zoom one level per notch.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WheelTravel(f32);

impl WheelTravel {
    /// A wheel event with vertical delta `delta_y` (up is positive) while
    /// Ctrl is `control`: the zoom step it completes, if any. Without Ctrl
    /// the wheel scrolls and the travel is forgotten.
    pub(crate) fn feed(&mut self, control: bool, delta_y: f32) -> Option<ZoomCommand> {
        if !control {
            self.0 = 0.0;
            return None;
        }
        // A direction change starts a fresh notch.
        if self.0 * delta_y < 0.0 {
            self.0 = 0.0;
        }
        self.0 += delta_y;
        if self.0.abs() < WHEEL_NOTCH {
            return None;
        }
        let command = if self.0 > 0.0 {
            ZoomCommand::In
        } else {
            ZoomCommand::Out
        };
        self.0 = 0.0;
        Some(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fileflux::core::thumbnails::Bucket;
    use fileflux::core::zoom::{CompactZoom, GridZoom, ListZoom, ZoomCommand};
    use gpui_component::Size;

    #[test]
    fn medium_grid_is_the_tile_of_before_the_zoom_existed() {
        let medium = GridZoom::Medium.metrics();
        assert_eq!(medium.tile_width, 112.0);
        assert_eq!(medium.tile_height(), 126.0);
        assert_eq!((medium.thumb_width, medium.thumb_height), (64.0, 52.0));
        assert_eq!(medium.glyph, 40.0);
        assert_eq!((medium.photo_width, medium.photo_height), (62.0, 46.0));
        assert_eq!(medium.photo_glyph, 28.0);
    }

    #[test]
    fn the_grid_size_table_grows_with_each_level() {
        let sizes = GridZoom::ALL.map(|zoom| {
            let m = zoom.metrics();
            (m.tile_width, m.tile_height(), m.thumb_width, m.thumb_height)
        });
        assert_eq!(
            sizes,
            [
                (88.0, 114.0, 48.0, 40.0),
                (112.0, 126.0, 64.0, 52.0),
                (160.0, 162.0, 112.0, 88.0),
                (224.0, 210.0, 176.0, 136.0),
            ]
        );
    }

    /// Everything a tile draws fits the box it is drawn in, at every level.
    #[test]
    fn every_level_fits_its_art_in_the_slot_and_the_slot_in_the_tile() {
        for zoom in GridZoom::ALL {
            let m = zoom.metrics();
            let folder_width = 59.0 * m.thumb_height / 50.0;
            assert!(folder_width <= m.thumb_width, "{zoom:?} folder");
            assert!(m.glyph <= m.thumb_height, "{zoom:?} glyph");
            assert!(m.photo_width <= m.thumb_width, "{zoom:?} photo width");
            assert!(m.photo_height <= m.thumb_height, "{zoom:?} photo height");
            assert!(m.photo_glyph < m.photo_height, "{zoom:?} photo glyph");
            assert!(m.thumb_width <= m.tile_width, "{zoom:?} slot");
        }
    }

    #[test]
    fn the_column_count_follows_the_tile_width_of_each_level() {
        // The inner width 1000 holds floor((1000 + gap) / (tile + gap)) tiles.
        let columns = GridZoom::ALL.map(|zoom| grid_columns(1000.0, zoom));
        assert_eq!(columns, [10, 8, 6, 4]);
        for zoom in GridZoom::ALL {
            let m = zoom.metrics();
            let two = 2.0 * m.tile_width + TILE_GAP;
            assert_eq!(grid_columns(two, zoom), 2, "{zoom:?} exactly two");
            assert_eq!(grid_columns(two - 1.0, zoom), 1, "{zoom:?} one short");
            assert_eq!(grid_columns(0.0, zoom), 1, "{zoom:?} never zero");
            assert_eq!(grid_columns(m.tile_width, zoom), 1, "{zoom:?} one tile");
        }
    }

    #[test]
    fn compact_cells_grow_with_the_level_and_medium_has_a_20_px_icon() {
        let sizes = CompactZoom::ALL.map(|zoom| {
            let m = zoom.metrics();
            (m.cell_width, m.cell_height, m.icon)
        });
        assert_eq!(
            sizes,
            [
                (150.0, 28.0, 16.0),
                (190.0, 32.0, 20.0),
                (240.0, 36.0, 24.0)
            ]
        );
        for zoom in CompactZoom::ALL {
            let m = zoom.metrics();
            assert!(m.icon < m.cell_height && m.icon < m.cell_width, "{zoom:?}");
        }
    }

    #[test]
    fn the_compact_column_count_follows_the_cell_width_of_each_level() {
        let columns = CompactZoom::ALL.map(|zoom| compact_columns(1000.0, zoom));
        assert_eq!(columns, [6, 5, 4]);
        for zoom in CompactZoom::ALL {
            let width = zoom.metrics().cell_width;
            let two = 2.0 * width + TILE_GAP;
            assert_eq!(compact_columns(two, zoom), 2, "{zoom:?} exactly two");
            assert_eq!(compact_columns(two - 1.0, zoom), 1, "{zoom:?} one short");
            assert_eq!(compact_columns(0.0, zoom), 1, "{zoom:?} never zero");
        }
    }

    #[test]
    fn up_and_down_move_by_a_row_of_cells_except_in_the_list() {
        assert_eq!(vertical_step(View::List, 7), 1);
        assert_eq!(vertical_step(View::Grid, 7), 7);
        assert_eq!(vertical_step(View::Compact, 5), 5);
        assert_eq!(vertical_step(View::Compact, 0), 1, "never zero");
    }

    #[test]
    fn the_list_rows_and_icons_come_from_the_level_and_medium_is_today() {
        assert_eq!(ListZoom::Medium.table_size(), Size::Small, "today's table");
        assert_eq!(ListZoom::Medium.icon(), 14.0, "today's glyph");
        let rows = ListZoom::ALL.map(ListZoom::row_height);
        assert_eq!(rows, [26.0, 30.0, 40.0]);
        for zoom in ListZoom::ALL {
            assert_eq!(
                zoom.row_height(),
                f32::from(zoom.table_size().table_row_height())
            );
            assert!(zoom.icon() < zoom.row_height());
        }
        let icons = ListZoom::ALL.map(ListZoom::icon);
        assert!(icons.is_sorted_by(|a, b| a < b), "{icons:?}");
    }

    #[test]
    fn thumbnails_ask_for_the_bucket_of_the_displayed_size() {
        let buckets = GridZoom::ALL.map(GridZoom::thumbnail_bucket);
        assert_eq!(
            buckets,
            [
                Bucket::Normal,
                Bucket::Normal,
                Bucket::Normal,
                Bucket::Large
            ]
        );
    }

    #[test]
    fn ctrl_wheel_up_zooms_in_down_zooms_out_and_plain_wheel_does_nothing() {
        let mut wheel = WheelTravel::default();
        assert_eq!(wheel.feed(true, WHEEL_NOTCH), Some(ZoomCommand::In));
        assert_eq!(wheel.feed(true, -WHEEL_NOTCH), Some(ZoomCommand::Out));
        assert_eq!(wheel.feed(true, 0.0), None);
        assert_eq!(wheel.feed(false, WHEEL_NOTCH), None, "no Ctrl: scrolls");
        assert_eq!(wheel.feed(false, -WHEEL_NOTCH), None);
    }

    #[test]
    fn small_wheel_deltas_add_up_to_a_notch_and_a_reversal_starts_over() {
        let mut wheel = WheelTravel::default();
        let tenth = WHEEL_NOTCH / 10.0;
        for _ in 0..9 {
            assert_eq!(wheel.feed(true, tenth), None);
        }
        assert_eq!(wheel.feed(true, tenth), Some(ZoomCommand::In));
        assert_eq!(wheel.feed(true, tenth), None, "the travel was spent");
        assert_eq!(wheel.feed(true, -tenth * 9.0), None, "reversal resets");
        assert_eq!(wheel.feed(true, tenth), None);
        assert_eq!(wheel.feed(true, -tenth * 9.0), None);
        assert_eq!(wheel.feed(true, -tenth * 5.0), Some(ZoomCommand::Out));
        // Releasing Ctrl forgets a half notch.
        assert_eq!(wheel.feed(true, tenth * 5.0), None);
        assert_eq!(wheel.feed(false, 1.0), None);
        assert_eq!(wheel.feed(true, tenth * 5.0), None);
    }
}
