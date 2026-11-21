# Fileflux design system

Derived from the component library the app actually renders with: **Bolt**, vendored from the DBFlux checkout at `/home/iperez/dev/personal/Rust/dbflux-worktree/main/crates/bolt`. Every value below that is marked with a path comes from that source, not from an estimate. Values marked **derived** are compositions of sourced tokens for a Fileflux-specific layout, and say so.

Read `design/system.html` for the same rules rendered.

## Sources

- `crates/bolt/src/tokens.rs` — sizes, spacing, heights, chamfers, menu and field metrics, shell geometry.
- `crates/bolt/src/colors/mod.rs` — neutral and semantic palette.
- `crates/bolt/src/brand.rs` — selection wash and tint.
- `crates/bolt/src/typography.rs`, `crates/bolt/src/fonts.rs` — families, weights, size scale.
- `crates/bolt/assets/fonts/` — the Archivo and JetBrains Mono faces the app embeds (OFL).
- `src/explorer/view.rs`, `src/explorer/sidebar.rs`, `src/explorer/usage_view.rs`, `src/explorer/rubber.rs` — Fileflux's own layout numbers.

## Colour

Neutral ladder, darkest to lightest. Source: `colors/mod.rs`.

| Token | Value | Usage |
|---|---|---|
| `--desk` | `#050507` | Ground behind the islands |
| `--background` | `#09090B` | Fields, segmented tracks, inspector preview ground |
| `--panel` | `#100F13` | Island surface |
| `--raised` | `#1A181E` | Hovered control fill, selected segment |
| `--line` | `#232128` | Hairline borders |
| `--line-2` | `#37333D` | Emphasis border, active edge |
| `--divider` | `#18161B` | Row separators |

Text and semantic colour.

| Token | Value | Usage |
|---|---|---|
| `--strong` | `#F7F4F7` | Primary text, selected rows, headings |
| `--foreground` | `#C6C3CC` | Body text |
| `--muted` | `#8E8996` | Secondary text, icons at rest |
| `--info` | `#6EA8FF` | Folder glyphs and non-error accents |
| `--success` | `#7BE0A0` | Available volume |
| `--warning` | `#FFC23D` | Estimates, cautions |
| `--danger` | `#FF6B5E` | Destructive actions and errors |
| `--hover` | `rgba(255,255,255,.04)` | Row and control hover |
| `--edge` | `rgba(255,255,255,.05)` | Island outline |

Selection. Source: `brand.rs`.

| Token | Value | Usage |
|---|---|---|
| `--tint` | `#9DB1FF` | Focus ring, 2px selection edge, active glyph |
| `--selection` | `rgba(157,177,255,.12)` | Selected row and tile wash |
| `--accent` | `#2B45B8` | Filled primary control only |

Rules: a selected row is the wash **plus** a 2px `--tint` inset on its leading edge, never a filled row. `--accent` is only for a filled primary button; navigation and folder glyphs use `--info`.

## Typography

Faces embedded by the app (`assets/fonts/`, OFL): **Archivo** 400/500/600/700 for UI, **Archivo Expanded** 800 for uppercase section labels, **JetBrains Mono** 400/500 for paths, sizes, permissions and any column that must align.

The previews in `design/` embed the same faces as subset latin woff2, so what you see is the metric the app draws.

| Role | Face | Size / line | Weight | Source |
|---|---|---|---|---|
| Body, list rows | Archivo | 13 / 18 | 500 | `src/explorer/*` row text |
| Field value, button label | Archivo | 12.5 / 16 | 600 | `tokens.rs:329`, `controls/button.rs:525` |
| Section label | Archivo Expanded | 10.5 / 14, `.14em`, uppercase | 800 | `tokens.rs:359-361` |
| Data column (size, date, permissions, path) | JetBrains Mono | 12 / 16 | 400 | details list meta |
| Table header | Archivo | 12 | 600 | `04-refined-shell.html`, table th |
| H1 in the content subbar | Archivo | 16 | 700 | subbar |

No letter-spacing outside the uppercase label. No italics in UI text.

## Geometry

Sourced heights (`tokens.rs:32-40`): row 28, compact row 24, header 40, toolbar 32, tab 36, input 30, button 30.

| Token | Value | Usage | Source |
|---|---|---|---|
| `--cut-island` | `14px` | Island top-left and bottom-right chamfer | `tokens.rs` island metrics |
| `--cut-control` | `6px` | Field, button and chip chamfer | `controls/button.rs:47-56` |
| `--cut-large` | `10px` | 44px call-to-action | `controls/button.rs` |
| `--focus` | `1.5px` | Keyboard focus ring, traced inside the chamfer | `tokens.rs:382` |
| `--edge` | `2px` | Selection edge | `tokens.rs` MEDIUM |
| spacing | 2, 4, 6, 8, 10, 12, 16, 24 | Gaps and padding | `tokens.rs:24-28` |

Fileflux shell (the app, not the library): sidebar 290 px (`tokens.rs:1769`), status bar 38 px (`tokens.rs:1787`), island gutter 8 px, sidebar clamps 200–600 px (`src/explorer/sidebar.rs:974`), grid tile 112 px (`src/explorer/rubber.rs:689`), usage row 28 px (`src/explorer/usage_view.rs:34`).

Corners are square. Only the listed chamfers are rounded, and only on the top-left and bottom-right corners.

## Components

Every component below is what the app draws. Sizes are the ones in the library; states are all the states the library renders.

### Button — `controls/button.rs`

| Size | Height | Cut | Padding x | Font | Use |
|---|---|---|---|---|---|
| Small | 24 | 6 | 10 | 12 | Inside table rows, chips, card rows (`button.rs:45-47`) |
| Medium | 30 | 6 | 12 | 12.5 | Toolbars, footers, dialogs, forms (`button.rs:55-56`, `tokens.rs:318`) |
| Large | 44 | 10 | 16 | 13 | A single call to action (`button.rs:51-52`, `tokens.rs:309`) |

States: rest (transparent fill, `--line` border where the control needs an edge), hover (`--raised` fill, `--line` border), focus-visible (`--tint` 1.5 px ring), pressed (`--raised` + `--line-2`), disabled (40 % opacity, no hover), danger (label and border `--danger`), primary (fill `--accent`, text `--on-accent`). Label weight 600.

### Icon button — same module

30×30 medium, 24×24 small, icon 14–16 px, no label, `aria-label` required, rest icon `--muted`, hover icon `--foreground`, pressed icon `--tint`.

### Text field and select trigger — `controls/input.rs`, `tokens.rs:305-331`

Height 30 (24 inline in dense toolbars, 44 large), padding x 12 (10 inline, 16 large), gap between icon/value/suffix 8, value text 12.5, background `--background`, border `--line`, focus border `--line-2` plus 1.5 px `--tint` ring. Placeholder `--muted`. A trailing keycap is 10.5 mono inside a `--line` border.

### Segmented control — `tokens.rs:423-431`

Track: `--background` fill, `--line` border, 2 px inner padding, 2 px gap. Segment: height 26, padding x 10, gap 6, icon 13. Selected segment: `--raised` fill and `--tint` glyph; unselected glyph `--muted`. Disabled: fill and content fade together (library rule).

### Menu and dropdown — `tokens.rs:448-456`

Panel: `--raised` fill, `--line-2` border, 8 px vertical padding, 6 px row inset, row height 30, max height 220 before it scrolls, highlighted row wash at 14 % tint. Section heading inside a menu: the uppercase label style. Separator: 1 px `--line`.

### Checkbox — `controls/checkbox.rs`

14 px box, `--line-2` border, checked: `--tint` border with `--selection` fill and a `--tint` glyph; label 12.5–13 beside it, line gap 3 px between label and its description. Mixed state draws a dash, never a filled box.

### Table — `src/explorer/table.rs` on Bolt's table

Header 34 px, header text 12/600 `--muted`, sticky, click to sort with a `--tint` arrow; row 28 px (compact 24); cell padding x 12; separators 1 px `--divider`; hover `--hover`; selected row: `--selection` wash plus a 2 px `--tint` leading edge. Numeric and mono columns right-aligned. The first column takes the remaining width and truncates with an ellipsis.

### Toast — `toast.rs`

Padding x 12 (`Feedback::TOAST_PADDING_X`), title 13 bold, body 12.5, meta 10.5 mono, 8 px gaps, stacks bottom-right, three at most. Live states: success, info, warning, error, with the accent taken from the matching semantic colour.

### Progress

Track 2–4 px `--line`, fill `--tint` for work in progress and `--info` for a measured share. Always paired with a mono label (`62%`, `184 210 folders left`) — never a bare bar.

### Breadcrumb / path bar — `04-refined-shell.html`

Height 30, `--background` fill, `--line` border, segments 13 px, ancestors `--muted`, current `--strong`, chevron 12 px `--line-2`, hover raises a segment to `--foreground`.

### Nav row and volume row — `src/explorer/sidebar.rs`

Nav row 26–28 px, 16 px horizontal padding, gap 10, glyph 15 `--muted` (active `--tint`), label 13; active row gets the `--selection` wash plus the 2 px `--tint` leading edge. Volume row is taller (auto) with a name line, a mono capacity line, a 3 px usage meter (`--line` track, `--info` fill) and a 22 px eject icon button.

### Island

`--panel` fill, 1 px `--edge` outline, 14 px chamfer top-left and bottom-right, 8 px gutter between islands, no shadow.

### Status bar — `tokens.rs:1787`

38 px, 12 px text, `--muted` labels with `--strong` values, 14 px gaps, right side for counters and state.

## Rules

1. Corners square; chamfer only islands (14), controls (6), large CTAs (10).
2. Selection = wash + 2 px tint edge. Never a filled row, never a border on all sides.
3. Hover and keyboard focus are different: hover changes fill, focus draws the 1.5 px tint ring inside the control.
4. One filled accent control per view at most; everything else is border or ghost.
5. Data that must align (sizes, dates, permissions, counts, paths) is JetBrains Mono and right-aligned; prose is Archivo.
6. Uppercase Archivo Expanded labels only for section headings, never for actions.
7. Icons are stroke, 24 viewBox, width 2, `currentColor`; size 13–16 inside controls.
8. Density is the library's: 28 px rows, 30 px controls, 8 px gaps. A screen that needs more room raises the number of columns, not the row height.
9. Destructive actions are `--danger` text with a confirmation, never a filled red button, unless the dialog is the only content.
10. Numbers never sit on top of a bar or an icon: a bar gets its own column, and the number its own right-aligned column.

## Not in Bolt

Marked so nobody fills the gap by invention: a sunburst or any chart primitive, a virtualised tree with connector lines, a sidebar section with a per-item action menu (the library has menus, not this composition), and a two-pane splitter. These are Fileflux compositions and must be specified in the feature's artboard before they are built.
