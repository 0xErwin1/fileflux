//! Drawing and pointer handling of the disk usage view: a sunburst chart
//! (rings are depth, the angle of a slice is its share of its parent) beside
//! the ranked list. The chart and the list share one selection.
//!
//! The layout of the chart is [`fileflux::core::usage_chart`]. The chart
//! follows the design of zex's disk usage view (Apache-2.0,
//! https://github.com/ejalxndr/zex): rings by depth, hover tooltip, click to
//! drill down, click in the centre to go up. The code is Fileflux's own.

use std::f32::consts::PI;
use std::path::PathBuf;
use std::sync::Arc;

use bolt::controls::Button;
use bolt::primitives::Icon;
use bolt::tokens::ChromeColors;
use fileflux::core::places::format_bytes;
use fileflux::core::usage::{Item, UsageTree};
use fileflux::core::usage_chart::{self, Segment, Target};
use gpui::{
    App, Bounds, ClickEvent, Context, DispatchPhase, Div, ElementId, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Path, PathBuilder,
    Pixels, Point, Stateful, Styled, WeakEntity, Window, canvas, div, point, prelude::*, px,
};
use gpui_component::ActiveTheme;

use super::icon;
use super::usage::{ChartHover, ShownKind, ShownRow, UsageSession, item_path};
use super::view::{ENTRIES_CONTEXT, Explorer};

/// The width of the chart column, its padding, and the side of the square
/// the chart is drawn in.
const CHART_COLUMN: f32 = 360.0;
const CHART_PADDING: f32 = 14.0;
const CHART_SIZE: f32 = CHART_COLUMN - 2.0 * CHART_PADDING;
/// The heights of the header row, the list head, one list row and the footer.
const HEADER_HEIGHT: f32 = 44.0;
const HEAD_HEIGHT: f32 = 34.0;
const ROW_HEIGHT: f32 = 28.0;
const FOOTER_HEIGHT: f32 = 38.0;
/// The horizontal padding of every list cell.
const CELL_PADDING: f32 = 12.0;
/// The widths of the share, size-on-disk and items columns, padding
/// included; the size column fits "999.9 GB".
const SHARE_WIDTH: f32 = 170.0;
const SIZE_WIDTH: f32 = 110.0;
const ITEMS_WIDTH: f32 = 96.0;
/// The chevron cell before a row's glyph.
const LEAD_WIDTH: f32 = 14.0;
/// The indentation of one level of an expanded folder.
const INDENT: f32 = 16.0;

fn hsla((h, s, l): (f32, f32, f32)) -> Hsla {
    Hsla { h, s, l, a: 1.0 }
}

fn lighter(color: Hsla) -> Hsla {
    Hsla {
        l: (color.l + 0.15).min(0.92),
        ..color
    }
}

/// The usage view: the header with its trail and buttons, then the chart
/// beside the list.
pub(crate) fn render(this: &Explorer, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let container = div()
        .id("explorer-usage")
        .key_context(ENTRIES_CONTEXT)
        .track_focus(this.entries_focus())
        .flex_1()
        .min_h_0()
        .min_w_0()
        .flex()
        .flex_col()
        .text_size(px(13.0));
    let Some(session) = this.usage() else {
        return container.justify_center().child(
            div()
                .id("explorer-usage-loading")
                .text_color(colors.muted)
                .child("Loading\u{2026}"),
        );
    };
    let weak = cx.entity().downgrade();
    let body = div()
        .id("explorer-usage-body")
        .flex_1()
        .min_h_0()
        .min_w_0()
        .flex()
        .child(chart_panel(this, session, &weak, cx))
        .child(list_panel(this, session, &weak, cx));
    container
        .child(header(session, cx))
        .child(body)
        .child(status(session, cx))
}

/// The trail of folders from the analysis root to the shown one (a click
/// shows that folder), and the Stop or Analyze again button.
fn header(session: &UsageSession, cx: &mut Context<Explorer>) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let mut chain: Vec<PathBuf> = session
        .shown
        .ancestors()
        .take_while(|path| path.starts_with(&session.analysis_root))
        .map(std::path::Path::to_path_buf)
        .collect();
    chain.reverse();
    let last = chain.len().saturating_sub(1);
    let mut trail = div()
        .id("explorer-usage-trail")
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap(px(6.0))
        .overflow_hidden()
        .font_family(bolt::fonts::editor_family(cx))
        .text_size(px(12.0));
    for (index, path) in chain.into_iter().enumerate() {
        let label = if index == 0 {
            path.to_string_lossy().into_owned()
        } else {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        if index > 0 {
            trail = trail.child(div().text_color(colors.line_2).child("/"));
        }
        let segment = div()
            .id(ElementId::Name(
                format!("explorer-usage-crumb-{index}").into(),
            ))
            .flex_none()
            .whitespace_nowrap()
            .text_color(if index == last {
                colors.strong
            } else {
                colors.muted
            })
            .child(label);
        trail = trail.child(if index == last || session.tree().is_none() {
            segment
        } else {
            segment
                .cursor_pointer()
                .hover(|style| style.text_color(colors.strong))
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.usage_drill(path.clone(), window, cx);
                }))
        });
    }
    let action = if session.is_walking() {
        Button::new("explorer-usage-stop", "Stop")
            .secondary()
            .icon(icon(gpui_kit_assets::IconName::Square))
            .tooltip("Stop measuring; the totals counted so far stay")
            .on_click(cx.listener(|this, _event, _window, cx| this.stop_usage(cx)))
    } else {
        Button::new("explorer-usage-again", "Analyze again")
            .secondary()
            .icon(icon(gpui_kit_assets::IconName::RotateCcw))
            .tooltip("Measure the folder again")
            .on_click(cx.listener(|this, _event, _window, cx| this.reanalyze_usage(cx)))
    };
    let mut header = div()
        .id("explorer-usage-header")
        .flex_none()
        .h(px(HEADER_HEIGHT))
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(20.0))
        .border_b_1()
        .border_color(colors.row_divider)
        .child(
            div()
                .flex_none()
                .text_size(px(16.0))
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(colors.strong)
                .child("Disk usage"),
        )
        .child(trail);
    if session.stale {
        header = header.child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .font_family(bolt::fonts::editor_family(cx))
                .text_size(px(11.0))
                .text_color(colors.warning)
                .child(
                    Icon::new(icon(gpui_kit_assets::IconName::TriangleAlert))
                        .size(px(12.0))
                        .color(colors.warning),
                )
                .child("freed space is an estimate"),
        );
    }
    header.child(action)
}

/// The footer: the totals of the shown folder and how much of the walk they
/// cover (the numbers are read here, not guessed from the chart), then the
/// counting rules.
fn status(session: &UsageSession, cx: &mut Context<Explorer>) -> gpui::AnyElement {
    let colors = bolt::colors(cx);
    let footer = div()
        .id("explorer-usage-status")
        .flex_none()
        .h(px(FOOTER_HEIGHT))
        .flex()
        .items_center()
        .gap(px(14.0))
        .px(px(16.0))
        .border_t_1()
        .border_color(colors.line)
        .text_size(px(12.0))
        .text_color(colors.muted);
    let rules = div()
        .ml_auto()
        .flex_none()
        .flex()
        .gap(px(14.0))
        .child("hard links count once")
        .child("links and other drives are not entered");
    let Some(summary) = session.summary() else {
        return footer.child(rules).into_any_element();
    };
    let totals = div()
        .flex_none()
        .flex()
        .gap(px(4.0))
        .child(
            div()
                .text_color(colors.foreground)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(format_bytes(summary.bytes)),
        )
        .child(format!("in {} items", summary.items));
    let mut text = String::new();
    if session.is_walking() {
        let rows = session.rows();
        let total = rows.len();
        let done = rows.iter().filter(|row| !row.pending).count();
        text.push_str(&format!("{done} of {total} children measured"));
    } else if summary.incomplete {
        text.push_str("some folders could not be read");
    }
    footer
        .child(totals)
        .when(!text.is_empty(), |footer| {
            footer.child(div().min_w_0().truncate().child(text))
        })
        .child(rules)
        .into_any_element()
}

/// The three largest slices, spelled out under the chart: the colour alone
/// says nothing without a name and a number.
fn legend(session: &UsageSession, cx: &mut Context<Explorer>) -> gpui::AnyElement {
    let colors = bolt::colors(cx);
    let rows: Vec<ShownRow> = session
        .rows()
        .into_iter()
        .filter(|row| row.depth == 0)
        .take(3)
        .collect();
    if rows.is_empty() {
        return div().into_any_element();
    }
    let mono = bolt::fonts::editor_family(cx);
    let mut list = div()
        .id("explorer-usage-legend")
        .flex_none()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .text_size(px(12.0));
    for (index, row) in rows.iter().enumerate() {
        let swatch = if row.kind == ShownKind::Other {
            colors.line_2
        } else {
            hsla(usage_chart::color(index, 1))
        };
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().flex_none().w(px(8.0)).h(px(8.0)).bg(swatch))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(colors.foreground)
                        .child(row.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .font_family(mono.clone())
                        .text_color(colors.strong)
                        .child(format_bytes(row.bytes)),
                ),
        );
    }
    list.into_any_element()
}

// --- the chart ---------------------------------------------------------------

fn chart_panel(
    this: &Explorer,
    session: &UsageSession,
    weak: &WeakEntity<Explorer>,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let panel = div()
        .id("explorer-usage-chart-panel")
        .flex_none()
        .w(px(CHART_COLUMN))
        .min_h_0()
        .p(px(CHART_PADDING))
        .border_r_1()
        .border_color(colors.line)
        .flex()
        .flex_col()
        .items_center()
        .justify_between()
        .gap(px(12.0));
    let Some(tree) = session.tree() else {
        // While the analysis runs there is no tree yet, but the per-child
        // totals measured so far are enough for one ring: the chart grows
        // with the measurement instead of waiting for it.
        let rows = session.rows();
        let shares: Vec<f32> = rows.iter().map(|row| row.share).collect();
        let segments = usage_chart::layout_rows(&shares);
        if segments.is_empty() {
            return panel.child(
                div()
                    .id("explorer-usage-chart-wait")
                    .h(px(CHART_SIZE))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(colors.muted)
                    .child(if session.is_walking() {
                        "Measuring\u{2026}"
                    } else {
                        "No chart"
                    }),
            );
        }
        let measured = rows
            .iter()
            .fold(0_u64, |sum, row| sum.saturating_add(row.bytes));
        let canvas = canvas(
            move |bounds, _window, _cx| bounds,
            move |bounds, _prepaint, window, _cx| {
                paint(bounds, &segments, None, None, window);
            },
        )
        .size_full();
        return panel
            .child(div().flex_1())
            .child(
                div()
                    .id("explorer-usage-chart")
                    .relative()
                    .w(px(CHART_SIZE))
                    .h(px(CHART_SIZE))
                    .child(canvas)
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(colors.strong)
                                    .child(format_bytes(measured)),
                            )
                            .child(
                                div()
                                    .font_family(bolt::fonts::editor_family(cx))
                                    .text_size(px(10.0))
                                    .text_color(colors.muted)
                                    .child("measuring"),
                            ),
                    ),
            )
            .child(div().flex_1())
            .child(legend(session, cx));
    };
    if session.segments.is_empty() {
        return panel.child(
            div()
                .id("explorer-usage-chart-empty")
                .h(px(CHART_SIZE))
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.muted)
                .child("Nothing to chart: no space is used here"),
        );
    }
    let segments = Arc::clone(&session.segments);
    let hovered = session.hover.map(|hover| hover.segment);
    let selected = session.selected_segment();
    let weak_for_events = weak.clone();
    let canvas = canvas(
        move |bounds, _window, _cx| bounds,
        move |bounds, _prepaint, window, _cx| {
            paint(bounds, &segments, hovered, selected, window);
            let hover = weak_for_events.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble
                    && let Err(error) = hover.update(cx, |this, cx| {
                        this.usage_chart_hover(bounds, event.position, cx);
                    })
                {
                    log::debug!("the chart outlived its explorer: {error}");
                }
            });
            let press = weak_for_events.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && bounds.contains(&event.position)
                    && let Err(error) = press.update(cx, |this, cx| {
                        this.usage_chart_press(bounds, event, window, cx);
                    })
                {
                    log::debug!("the chart outlived its explorer: {error}");
                }
            });
        },
    )
    .size_full();
    let root = tree.find(&session.shown).unwrap_or(UsageTree::ROOT);
    let centre_name = session
        .shown
        .file_name()
        .map_or_else(
            || session.shown.to_string_lossy(),
            |name| name.to_string_lossy(),
        )
        .into_owned();
    let tooltip = session
        .hover
        .and_then(|hover| chart_tooltip(tree, session, hover, cx));
    let _ = this;
    panel
        .child(div().flex_1())
        .child(
            div()
                .id("explorer-usage-chart")
                .relative()
                .w(px(CHART_SIZE))
                .h(px(CHART_SIZE))
                .child(canvas)
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(2.0))
                        .child(
                            div()
                                .max_w(px(120.0))
                                .truncate()
                                .text_size(px(15.0))
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(colors.strong)
                                .child(centre_name),
                        )
                        .child(
                            div()
                                .font_family(bolt::fonts::editor_family(cx))
                                .text_size(px(11.0))
                                .text_color(colors.muted)
                                .child(format_bytes(tree.total(root))),
                        ),
                )
                .children(tooltip),
        )
        .child(div().flex_1())
        .child(legend(session, cx))
}

fn segment_color(segment: &Segment, highlighted: bool) -> Hsla {
    let color = match segment.target {
        // The design's line-2 grey: the folded rest reads as background, not
        // as one more slice.
        Target::Other { .. } | Target::Item(Item::OtherFiles(_)) => Hsla {
            h: 0.76,
            s: 0.08,
            l: 0.28,
            a: 1.0,
        },
        Target::Item(_) | Target::Row(_) => hsla(usage_chart::color(segment.branch, segment.depth)),
    };
    if highlighted { lighter(color) } else { color }
}

fn chart_geometry(bounds: Bounds<Pixels>) -> (Point<Pixels>, f32) {
    let radius = f32::from(bounds.size.width).min(f32::from(bounds.size.height)) / 2.0 * 0.95;
    (bounds.center(), radius)
}

fn paint(
    bounds: Bounds<Pixels>,
    segments: &[Segment],
    hovered: Option<usize>,
    selected: Option<usize>,
    window: &mut Window,
) {
    let (center, max_radius) = chart_geometry(bounds);
    for (index, segment) in segments.iter().enumerate() {
        let color = segment_color(segment, Some(index) == hovered || Some(index) == selected);
        if let Some(path) = segment_path(center, max_radius, segment) {
            window.paint_path(path, color);
        }
    }
}

fn on_circle(center: Point<Pixels>, radius: f32, angle: f32) -> Point<Pixels> {
    point(
        center.x + px(radius * angle.sin()),
        center.y - px(radius * angle.cos()),
    )
}

fn segment_path(center: Point<Pixels>, max_radius: f32, segment: &Segment) -> Option<Path<Pixels>> {
    let inner = segment.inner * max_radius;
    let outer = segment.outer * max_radius;
    // A full circle is one arc that cannot say where it ends: stop a hair
    // short of the start.
    let end = segment.end.min(segment.start + 2.0 * PI - 1e-3);
    let large = end - segment.start > PI;
    let mut builder = PathBuilder::fill();
    builder.move_to(on_circle(center, inner, segment.start));
    builder.line_to(on_circle(center, outer, segment.start));
    builder.arc_to(
        point(px(outer), px(outer)),
        px(0.0),
        large,
        true,
        on_circle(center, outer, end),
    );
    builder.line_to(on_circle(center, inner, end));
    builder.arc_to(
        point(px(inner), px(inner)),
        px(0.0),
        large,
        false,
        on_circle(center, inner, segment.start),
    );
    builder.close();
    builder.build().ok()
}

fn chart_tooltip(
    tree: &UsageTree,
    session: &UsageSession,
    hover: ChartHover,
    cx: &mut Context<Explorer>,
) -> Option<gpui::AnyElement> {
    let colors = bolt::colors(cx);
    let segment = session.segments.get(hover.segment)?;
    let (name, bytes, items) = match segment.target {
        Target::Item(item) => {
            let row = tree.row(item, segment.bytes(tree), segment.parent_bytes);
            (
                row.name.to_string_lossy().into_owned(),
                row.bytes,
                row.items,
            )
        }
        Target::Other { bytes, count, .. } => ("Other".to_owned(), bytes, count),
        // Only the chart drawn while measuring carries row segments, and it
        // has no tooltip.
        Target::Row(_) => return None,
    };
    let share = bytes as f64 / segment.parent_bytes.max(1) as f64 * 100.0;
    Some(
        div()
            .id("explorer-usage-tooltip")
            .absolute()
            .left(hover.at.x + px(12.0))
            .top(hover.at.y + px(12.0))
            .max_w(px(240.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(8.0))
            .bg(colors.raised)
            .border_1()
            .border_color(colors.line)
            .child(div().text_color(colors.strong).truncate().child(name))
            .child(
                div()
                    .text_color(colors.muted)
                    .child(format!("{} \u{b7} {share:.1}%", format_bytes(bytes))),
            )
            .when(items > 0, |tip| {
                tip.child(
                    div()
                        .text_color(colors.muted)
                        .child(format!("{items} items")),
                )
            })
            .into_any_element(),
    )
}

impl Explorer {
    /// The pointer moved over (or off) the chart.
    pub(crate) fn usage_chart_hover(
        &mut self,
        bounds: Bounds<Pixels>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.tab_mut().usage.as_mut() else {
            return;
        };
        let found =
            chart_hit(bounds, position, &session.segments).and_then(|hit| hit.segment_index);
        let next = found.map(|segment| ChartHover {
            segment,
            at: point(position.x - bounds.origin.x, position.y - bounds.origin.y),
        });
        match (&session.hover, &next) {
            (None, None) => {}
            (Some(old), Some(new)) if old.segment == new.segment && old.at == new.at => {}
            _ => {
                session.hover = next;
                cx.notify();
            }
        }
    }

    /// A press on the chart: a left press drills into a folder slice, selects
    /// a file slice, goes up from the centre; a right press opens the menu
    /// of the slice's item.
    pub(crate) fn usage_chart_press(
        &mut self,
        bounds: Bounds<Pixels>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.tab().usage.as_ref() else {
            return;
        };
        let Some(tree) = session.tree() else {
            return;
        };
        let Some(hit) = chart_hit(bounds, event.position, &session.segments) else {
            return;
        };
        let up = session.up_target();
        let shown = session.shown.clone();
        let target = hit
            .segment_index
            .and_then(|index| session.segments.get(index))
            .map(|segment| segment.target);
        let item = match target {
            Some(Target::Item(item)) => Some((item_path(tree, item), item)),
            _ => None,
        };
        let other_dir = match target {
            Some(Target::Other { dir, .. }) => Some(tree.path_of(dir)),
            _ => None,
        };
        self.entries_focus().focus(window, cx);
        match event.button {
            MouseButton::Left if hit.center => {
                if let Some(parent) = up {
                    self.usage_drill(parent, window, cx);
                }
            }
            MouseButton::Left => {
                if let Some((path, Item::Folder(_))) = item {
                    self.usage_click(path.clone(), 1, window, cx);
                    self.usage_activate(&path, window, cx);
                } else if let Some((path, _)) = item {
                    self.usage_click(path, event.click_count, window, cx);
                } else if let Some(dir) = other_dir.filter(|dir| *dir != shown) {
                    self.usage_drill(dir, window, cx);
                }
            }
            MouseButton::Right => {
                if let Some((path, _)) = item {
                    self.open_usage_menu(event.position, &path, window, cx);
                }
            }
            _ => {}
        }
    }
}

/// What a point of the chart is over.
struct ChartHit {
    /// The empty centre.
    center: bool,
    /// Index of the slice under the point, if one.
    segment_index: Option<usize>,
}

fn chart_hit(
    bounds: Bounds<Pixels>,
    position: Point<Pixels>,
    segments: &[Segment],
) -> Option<ChartHit> {
    let (center, max_radius) = chart_geometry(bounds);
    let offset = (
        f32::from(position.x - center.x),
        f32::from(position.y - center.y),
    );
    let (angle, radius) = usage_chart::polar(offset, max_radius);
    if usage_chart::in_center(radius) {
        return Some(ChartHit {
            center: true,
            segment_index: None,
        });
    }
    let found = usage_chart::hit_test(segments, angle, radius)?;
    Some(ChartHit {
        center: false,
        segment_index: segments
            .iter()
            .position(|segment| std::ptr::eq(segment, found)),
    })
}

// --- the list ----------------------------------------------------------------

fn list_panel(
    this: &Explorer,
    session: &UsageSession,
    weak: &WeakEntity<Explorer>,
    cx: &mut Context<Explorer>,
) -> Stateful<Div> {
    let colors = bolt::colors(cx);
    let rows: Arc<Vec<ShownRow>> = Arc::new(session.rows());
    let mut panel = div()
        .id("explorer-usage-list-panel")
        .flex_1()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .flex()
        .flex_col();
    if rows.is_empty() {
        let note = match (&session.phase, session.is_walking()) {
            (super::usage::Phase::Failed(message), _) => format!("Could not analyze: {message}"),
            (_, true) => "Measuring\u{2026}".to_owned(),
            _ => "This folder is empty".to_owned(),
        };
        return panel.justify_center().items_center().child(
            div()
                .id("explorer-usage-empty")
                .text_color(colors.muted)
                .child(note),
        );
    }
    let head_cell = |width: f32, label: &'static str| {
        div()
            .flex_none()
            .w(px(width))
            .px(px(CELL_PADDING))
            .whitespace_nowrap()
            .text_right()
            .child(label)
    };
    panel = panel.child(
        div()
            .id("explorer-usage-head")
            .flex_none()
            .h(px(HEAD_HEIGHT))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(colors.line)
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(colors.muted)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(CELL_PADDING))
                    .truncate()
                    .child("Name"),
            )
            .child(head_cell(SHARE_WIDTH, "Share"))
            .child(head_cell(SIZE_WIDTH, "Size on disk"))
            .child(head_cell(ITEMS_WIDTH, "Items")),
    );
    let selected = session.selected.clone();
    let actionable = session.tree().is_some();
    let weak_for_rows = weak.clone();
    let count = rows.len();
    let _ = this;
    panel.child(
        gpui::uniform_list(
            "explorer-usage-rows",
            count,
            move |range, _window, cx: &mut App| {
                range
                    .filter_map(|index| rows.get(index))
                    .map(|row| {
                        render_row(
                            row,
                            selected.as_ref() == Some(&row.path),
                            actionable,
                            &weak_for_rows,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&session.list_scroll.clone()),
    )
}

fn render_row(
    row: &ShownRow,
    selected: bool,
    actionable: bool,
    weak: &WeakEntity<Explorer>,
    cx: &mut App,
) -> gpui::AnyElement {
    let colors = bolt::colors(cx);
    let tint = ChromeColors::tint(cx.theme());
    let (glyph, glyph_color) = match row.kind {
        ShownKind::Folder => (gpui_kit_assets::IconName::Folder, colors.info),
        ShownKind::File => (gpui_kit_assets::IconName::File, colors.muted),
        ShownKind::Other => (gpui_kit_assets::IconName::Files, colors.muted),
    };
    let path = row.path.clone();
    let id = ElementId::Name(format!("explorer-usage-row-{}", path.to_string_lossy()).into());
    let chevron = if row.expandable && actionable {
        let toggle_path = row.path.clone();
        let weak = weak.clone();
        div()
            .id(ElementId::Name(
                format!("explorer-usage-chevron-{}", path.to_string_lossy()).into(),
            ))
            .flex_none()
            .w(px(LEAD_WIDTH))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_click(move |_event, _window, cx| {
                cx.stop_propagation();
                if let Err(error) = weak.update(cx, |this, cx| this.usage_toggle(&toggle_path, cx))
                {
                    log::debug!("the row outlived its explorer: {error}");
                }
            })
            .child(
                Icon::new(icon(if row.expanded {
                    gpui_kit_assets::IconName::ChevronDown
                } else {
                    gpui_kit_assets::IconName::ChevronRight
                }))
                .size(px(12.0))
                .color(colors.muted),
            )
    } else {
        div()
            .id(ElementId::Name(
                format!("explorer-usage-spacer-{}", path.to_string_lossy()).into(),
            ))
            .flex_none()
            .w(px(LEAD_WIDTH))
    };
    let mut name = row.name.clone();
    if row.incomplete {
        name.push_str(" (incomplete)");
    }
    let bar_width = SHARE_WIDTH - 2.0 * CELL_PADDING;
    let bar_fill =
        (row.share.clamp(0.0, 1.0) * bar_width).max(if row.bytes > 0 { 2.0 } else { 0.0 });
    let mono = bolt::fonts::editor_family(cx);
    let number_cell = |width: f32| {
        div()
            .flex_none()
            .w(px(width))
            .px(px(CELL_PADDING))
            .whitespace_nowrap()
            .text_right()
            .font_family(mono.clone())
            .text_size(px(12.0))
    };
    let mut element = div()
        .id(id)
        .relative()
        .w_full()
        .h(px(ROW_HEIGHT))
        .flex_none()
        .overflow_hidden()
        .flex()
        .items_center()
        .border_b_1()
        .border_color(colors.row_divider)
        .when(selected, |element| {
            element.bg(tint.opacity(0.12)).child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(tint),
            )
        })
        .hover(|style| style.bg(colors.hover))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(8.0))
                .pl(px(CELL_PADDING - 4.0 + row.depth as f32 * INDENT))
                .pr(px(CELL_PADDING))
                .child(chevron)
                .child(Icon::new(icon(glyph)).size(px(14.0)).color(glyph_color))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if row.pending {
                            colors.muted
                        } else if selected {
                            colors.strong
                        } else {
                            colors.foreground
                        })
                        .child(name),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(SHARE_WIDTH))
                .px(px(CELL_PADDING))
                .child(div().w_full().h(px(6.0)).bg(colors.line).child(
                    div().h_full().w(px(bar_fill)).bg(if row.pending {
                        colors.muted
                    } else {
                        colors.info
                    }),
                )),
        )
        .child(
            number_cell(SIZE_WIDTH)
                .text_color(colors.strong)
                .child(format_bytes(row.bytes)),
        )
        .child(
            number_cell(ITEMS_WIDTH)
                .text_color(colors.muted)
                .child(match row.kind {
                    ShownKind::File => "\u{2014}".to_owned(),
                    _ => row.items.to_string(),
                }),
        );
    if actionable && row.kind != ShownKind::Other {
        let click_path = row.path.clone();
        let click_weak = weak.clone();
        let menu_path = row.path.clone();
        let menu_weak = weak.clone();
        element = element
            .on_click(move |event: &ClickEvent, window, cx| {
                let count = event.click_count();
                if let Err(error) = click_weak.update(cx, |this, cx| {
                    this.usage_click(click_path.clone(), count, window, cx);
                }) {
                    log::debug!("the row outlived its explorer: {error}");
                }
            })
            .on_mouse_down(
                MouseButton::Right,
                move |event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if let Err(error) = menu_weak.update(cx, |this, cx| {
                        this.entries_focus().focus(window, cx);
                        this.open_usage_menu(event.position, &menu_path, window, cx);
                    }) {
                        log::debug!("the row outlived its explorer: {error}");
                    }
                },
            );
    }
    element.into_any_element()
}
