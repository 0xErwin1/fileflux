//! The geometry of the disk usage sunburst: rings are depth, the angle of a
//! segment is its share of its parent. Pure: the explorer paints the
//! segments and maps the pointer back through [`hit_test`].
//!
//! The approach (rings by depth, slices thinner than a threshold folded into
//! one "other" segment so the count stays bounded) follows the sunburst of
//! zex (Apache-2.0, https://github.com/ejalxndr/zex); the code is written
//! for Fileflux's model, not copied.

use std::f32::consts::PI;

use super::usage::{Item, NodeId, UsageTree};

/// How many rings the chart draws below the root.
pub const RING_DEPTH: u32 = 3;
/// The radius of the empty centre, as a fraction of the chart radius.
pub const CENTER_HOLE: f32 = 0.22;
/// Segments thinner than this (radians) are folded into one "other" segment
/// of their parent, which bounds the segment count however wide a folder is.
pub const MIN_SEGMENT_ANGLE: f32 = 0.025;

const TAU: f32 = 2.0 * PI;

/// What a segment stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Item(Item),
    /// The children of `dir` too thin to draw, together.
    Other {
        dir: NodeId,
        bytes: u64,
        count: u64,
    },
    /// A row of the list while the analysis runs: there is no tree yet, so
    /// the segment stands for one measured child, by its position in the
    /// ranked list.
    Row(usize),
}

/// One drawn slice of a ring. Angles run clockwise from twelve o'clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub target: Target,
    /// 1 for the children of the root.
    pub depth: u32,
    /// The index of the top-level branch it belongs to: its colour.
    pub branch: usize,
    pub start: f32,
    pub end: f32,
    /// Radii as fractions of the chart radius.
    pub inner: f32,
    pub outer: f32,
    /// The bytes of the segment's siblings together (the share's base).
    pub parent_bytes: u64,
}

impl Segment {
    #[must_use]
    pub fn bytes(&self, tree: &UsageTree) -> u64 {
        match self.target {
            Target::Row(_) => 0,
            Target::Item(Item::Folder(id)) => tree.total(id),
            Target::Item(item) => tree
                .items(item_dir(item))
                .into_iter()
                .find(|(candidate, _)| *candidate == item)
                .map_or(0, |(_, bytes)| bytes),
            Target::Other { bytes, .. } => bytes,
        }
    }
}

fn item_dir(item: Item) -> NodeId {
    match item {
        Item::Folder(id) | Item::OtherFiles(id) => id,
        Item::File { dir, .. } => dir,
    }
}

/// The segments of the rings under `root`, down to `max_depth` rings.
#[must_use]
pub fn layout(tree: &UsageTree, root: NodeId, max_depth: u32) -> Vec<Segment> {
    let mut out = Vec::new();
    layout_children(tree, root, None, (0.0, TAU), 1, max_depth, &mut out);
    out
}

fn layout_children(
    tree: &UsageTree,
    dir: NodeId,
    branch: Option<usize>,
    (start, end): (f32, f32),
    depth: u32,
    max_depth: u32,
    out: &mut Vec<Segment>,
) {
    if depth > max_depth {
        return;
    }
    let children: Vec<(Item, u64)> = tree
        .items(dir)
        .into_iter()
        .filter(|(_, bytes)| *bytes > 0)
        .collect();
    let total: u64 = children.iter().map(|(_, bytes)| bytes).sum();
    if total == 0 {
        return;
    }
    let ring = (1.0 - CENTER_HOLE) / max_depth.max(1) as f32;
    let inner = CENTER_HOLE + (depth - 1) as f32 * ring;
    let outer = inner + ring;
    let span = end - start;
    let mut cursor = start;
    let mut drawn = 0usize;
    let mut drawn_bytes = 0u64;
    for (index, (item, bytes)) in children.iter().enumerate() {
        let width = span * (*bytes as f32 / total as f32);
        // Largest first: everything after the first thin slice is thinner.
        if width < MIN_SEGMENT_ANGLE {
            break;
        }
        let branch = branch.unwrap_or(index);
        out.push(Segment {
            target: Target::Item(*item),
            depth,
            branch,
            start: cursor,
            end: cursor + width,
            inner,
            outer,
            parent_bytes: total,
        });
        if let Item::Folder(child) = item {
            layout_children(
                tree,
                *child,
                Some(branch),
                (cursor, cursor + width),
                depth + 1,
                max_depth,
                out,
            );
        }
        cursor += width;
        drawn += 1;
        drawn_bytes += bytes;
    }
    if drawn < children.len() {
        out.push(Segment {
            target: Target::Other {
                dir,
                bytes: total - drawn_bytes,
                count: (children.len() - drawn) as u64,
            },
            depth,
            branch: branch.unwrap_or(drawn),
            start: cursor,
            end,
            inner,
            outer,
            parent_bytes: total,
        });
    }
}

/// A pointer offset from the centre as an angle (clockwise from twelve
/// o'clock, 0 to 2π) and a radius as a fraction of `max_radius`.
#[must_use]
pub fn polar(offset: (f32, f32), max_radius: f32) -> (f32, f32) {
    let (dx, dy) = offset;
    let radius = dx.hypot(dy);
    let angle = (dy.atan2(dx) + PI / 2.0).rem_euclid(TAU);
    let fraction = if max_radius > 0.0 {
        radius / max_radius
    } else {
        0.0
    };
    (angle, fraction)
}

/// The segment under a point given as [`polar`] returns it.
#[must_use]
pub fn hit_test(segments: &[Segment], angle: f32, radius: f32) -> Option<&Segment> {
    segments.iter().find(|segment| {
        radius >= segment.inner
            && radius < segment.outer
            && angle >= segment.start
            && angle < segment.end
    })
}

/// Whether a point (as [`polar`] returns it) is in the empty centre.
#[must_use]
pub fn in_center(radius: f32) -> bool {
    radius < CENTER_HOLE
}

/// The colour of a segment as hue (0 to 1), saturation and lightness: a
/// golden-angle hue per top-level branch, darker per ring.
#[must_use]
pub fn color(branch: usize, depth: u32) -> (f32, f32, f32) {
    const GOLDEN_ANGLE: f32 = 137.507_77;
    let hue = (branch as f32 * GOLDEN_ANGLE).rem_euclid(360.0) / 360.0;
    let lightness = (0.55 - depth.saturating_sub(1) as f32 * 0.07).max(0.28);
    (hue, 0.55, lightness)
}

/// One ring for the chart shown while the analysis runs: the shares measured
/// so far, in list order, each `Row(index)` into that list. The ring always
/// closes on itself, so the chart reads as a whole even when the numbers are
/// still growing.
#[must_use]
pub fn layout_rows(shares: &[f32]) -> Vec<Segment> {
    let total: f32 = shares.iter().filter(|share| **share > 0.0).sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0.0;
    for (index, share) in shares.iter().enumerate() {
        if *share <= 0.0 {
            continue;
        }
        let sweep = share / total * TAU;
        out.push(Segment {
            target: Target::Row(index),
            depth: 1,
            branch: index,
            start,
            end: start + sweep,
            inner: CENTER_HOLE,
            outer: 1.0,
            parent_bytes: 0,
        });
        start += sweep;
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// The ring drawn while measuring closes on itself and keeps every row
    /// addressable, so the chart reads as a whole and grows with the walk.
    #[test]
    fn the_measured_ring_closes_and_keeps_every_row() {
        let segments = layout_rows(&[0.5, 0.0, 0.25, 0.25]);
        assert_eq!(segments.len(), 3, "a zero share draws nothing");
        let first = &segments[0];
        assert_eq!(first.start, 0.0);
        assert!((segments[2].end - TAU).abs() < 1e-5, "the ring closes");
        assert_eq!(segments[0].target, Target::Row(0));
        assert_eq!(segments[1].target, Target::Row(2), "the index is the row's");
        assert!(
            segments
                .windows(2)
                .all(|w| (w[0].end - w[1].start).abs() < 1e-6)
        );
    }

    /// Shares are relative to what has been measured, not to a guessed total,
    /// so a half-measured tree still fills the ring.
    #[test]
    fn a_partial_measurement_still_fills_the_ring() {
        let segments = layout_rows(&[1.0, 3.0]);
        let sweep: f32 = segments
            .iter()
            .map(|segment| segment.end - segment.start)
            .sum();
        assert!((sweep - TAU).abs() < 1e-5);
        assert!((segments[0].end - segments[0].start - TAU / 4.0).abs() < 1e-5);
        assert!(layout_rows(&[]).is_empty());
        assert!(layout_rows(&[0.0, 0.0]).is_empty());
    }
    use crate::core::usage::tests::{Scratch, run};
    use std::fs;

    fn segment_of<'a>(
        segments: &'a [Segment],
        tree: &UsageTree,
        name: &str,
    ) -> Option<&'a Segment> {
        segments.iter().find(|segment| match segment.target {
            Target::Item(Item::Folder(id)) => {
                tree.path_of(id).file_name().is_some_and(|n| n == name)
            }
            Target::Item(Item::File { dir, index }) => {
                tree.row(Item::File { dir, index }, 0, 0).name == name
            }
            _ => false,
        })
    }

    #[test]
    fn angles_follow_the_share_of_the_parent_and_fill_the_circle() -> std::io::Result<()> {
        let scratch = Scratch::new("chart-angles")?;
        fs::write(scratch.path().join("big"), vec![1u8; 300_000])?;
        fs::write(scratch.path().join("small"), vec![1u8; 100_000])?;
        let tree = run(scratch.path())?;
        let segments = layout(&tree, UsageTree::ROOT, 1);
        assert_eq!(segments.len(), 2);
        let big =
            segment_of(&segments, &tree, "big").ok_or_else(|| std::io::Error::other("big"))?;
        let small =
            segment_of(&segments, &tree, "small").ok_or_else(|| std::io::Error::other("small"))?;
        let rows = tree.rows(UsageTree::ROOT);
        let children: u64 = rows.iter().map(|row| row.bytes).sum();
        let want = (rows[0].bytes as f32 / children as f32) * TAU;
        assert!((big.end - big.start - want).abs() < 1e-3);
        assert_eq!(big.start, 0.0);
        assert!((big.end - small.start).abs() < 1e-6);
        assert!((small.end - TAU).abs() < 1e-3);
        assert_eq!(big.parent_bytes, children);
        Ok(())
    }

    #[test]
    fn deeper_rings_sit_outside_and_inside_their_parent_span() -> std::io::Result<()> {
        let scratch = Scratch::new("chart-rings")?;
        fs::create_dir_all(scratch.path().join("mid/leafdir"))?;
        fs::write(scratch.path().join("mid/leafdir/f"), vec![1u8; 200_000])?;
        let tree = run(scratch.path())?;
        let segments = layout(&tree, UsageTree::ROOT, RING_DEPTH);
        let mid =
            segment_of(&segments, &tree, "mid").ok_or_else(|| std::io::Error::other("mid"))?;
        let leafdir = segment_of(&segments, &tree, "leafdir")
            .ok_or_else(|| std::io::Error::other("leafdir"))?;
        let file = segment_of(&segments, &tree, "f").ok_or_else(|| std::io::Error::other("f"))?;
        assert_eq!((mid.depth, leafdir.depth, file.depth), (1, 2, 3));
        assert!((mid.outer - leafdir.inner).abs() < 1e-6);
        assert!((leafdir.outer - file.inner).abs() < 1e-6);
        assert!(file.outer <= 1.0 + 1e-6 && mid.inner >= CENTER_HOLE - 1e-6);
        assert!(leafdir.start >= mid.start - 1e-6 && leafdir.end <= mid.end + 1e-6);
        assert_eq!(mid.branch, leafdir.branch);
        Ok(())
    }

    #[test]
    fn rings_stop_at_the_depth_cap() -> std::io::Result<()> {
        let scratch = Scratch::new("chart-cap")?;
        fs::create_dir_all(scratch.path().join("a/b/c/d"))?;
        fs::write(scratch.path().join("a/b/c/d/f"), vec![1u8; 100_000])?;
        let tree = run(scratch.path())?;
        let segments = layout(&tree, UsageTree::ROOT, 2);
        assert_eq!(segments.iter().map(|s| s.depth).max(), Some(2));
        assert!(segment_of(&segments, &tree, "c").is_none());
        Ok(())
    }

    #[test]
    fn thin_slices_fold_into_one_other_segment() -> std::io::Result<()> {
        let scratch = Scratch::new("chart-other")?;
        for index in 0..150 {
            fs::write(scratch.path().join(format!("t{index:03}")), b"x")?;
        }
        fs::write(scratch.path().join("big"), vec![1u8; 4_000_000])?;
        let tree = run(scratch.path())?;
        let segments = layout(&tree, UsageTree::ROOT, 1);
        assert_eq!(segments.len(), 2, "the big file and one fold");
        let other = segments
            .iter()
            .find_map(|segment| match segment.target {
                Target::Other { dir, bytes, count } => Some((dir, bytes, count)),
                Target::Item(_) | Target::Row(_) => None,
            })
            .ok_or_else(|| std::io::Error::other("no fold"))?;
        assert_eq!(other.0, UsageTree::ROOT);
        assert_eq!(other.2, 150);
        let tiny: u64 = tree
            .rows(UsageTree::ROOT)
            .iter()
            .filter(|row| row.name != "big")
            .map(|row| row.bytes)
            .sum();
        assert_eq!(other.1, tiny);
        assert!(segments.last().is_some_and(|s| (s.end - TAU).abs() < 1e-3));
        Ok(())
    }

    #[test]
    fn an_empty_or_zero_sized_folder_has_no_segments() -> std::io::Result<()> {
        let scratch = Scratch::new("chart-empty")?;
        let tree = run(scratch.path())?;
        assert!(layout(&tree, UsageTree::ROOT, RING_DEPTH).is_empty());
        Ok(())
    }

    #[test]
    fn a_point_maps_back_to_its_segment_and_the_centre_hits_nothing() -> std::io::Result<()> {
        let scratch = Scratch::new("chart-hit")?;
        fs::write(scratch.path().join("big"), vec![1u8; 300_000])?;
        fs::write(scratch.path().join("small"), vec![1u8; 100_000])?;
        let tree = run(scratch.path())?;
        let segments = layout(&tree, UsageTree::ROOT, 1);
        let big =
            segment_of(&segments, &tree, "big").ok_or_else(|| std::io::Error::other("big"))?;
        let angle = (big.start + big.end) / 2.0;
        let radius = (big.inner + big.outer) / 2.0;
        assert_eq!(hit_test(&segments, angle, radius), Some(big));
        assert_eq!(hit_test(&segments, angle, CENTER_HOLE / 2.0), None);
        assert_eq!(hit_test(&segments, angle, 1.2), None);
        assert!(in_center(CENTER_HOLE / 2.0) && !in_center(radius));
        Ok(())
    }

    #[test]
    fn polar_measures_clockwise_from_twelve_oclock() {
        let (angle, radius) = polar((0.0, -50.0), 100.0);
        assert!(angle.abs() < 1e-6 && (radius - 0.5).abs() < 1e-6);
        let (angle, _) = polar((50.0, 0.0), 100.0);
        assert!((angle - PI / 2.0).abs() < 1e-6, "three o'clock");
        let (angle, _) = polar((0.0, 50.0), 100.0);
        assert!((angle - PI).abs() < 1e-6, "six o'clock");
        let (angle, _) = polar((-50.0, 0.0), 100.0);
        assert!((angle - 1.5 * PI).abs() < 1e-5, "nine o'clock");
        assert_eq!(polar((3.0, 4.0), 0.0).1, 0.0);
    }

    #[test]
    fn branches_get_distinct_hues_and_deeper_rings_are_darker() {
        assert_ne!(color(0, 1).0, color(1, 1).0);
        assert!(color(0, 3).2 < color(0, 1).2);
        assert!(color(0, 99).2 >= 0.28);
    }
}
