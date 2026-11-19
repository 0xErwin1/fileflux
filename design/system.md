# Fileflux · Bolt family design system

Inherited from Bolt, with DBFlux and ReqFlux as product references. This is a design-only translation, not a GPUI implementation.

## Sources
- /home/iperez/dev/personal/Rust/dbflux-worktree/main/crates/bolt/src/colors/mod.rs — neutral and semantic palette.
- /home/iperez/dev/personal/Rust/dbflux-worktree/main/crates/bolt/src/brand.rs:33–75 — selection washes.
- /home/iperez/dev/personal/Rust/dbflux-worktree/main/crates/bolt/src/typography.rs:18–26 — font families.
- /home/iperez/dev/personal/Rust/dbflux-worktree/main/crates/bolt/src/tokens.rs:1717–1793,1979–1990 — shell and islands.
- /home/iperez/dev/personal/Rust/reqflux/crates/reqflux_ui/src/shell/path_bar.rs:16–40 — path navigation.
- /home/iperez/dev/personal/Rust/reqflux/design/bolt-v2/01-main.html:11–43 — checked HTML translation and tree rows.

## Colors
| Token | Value | Usage |
|---|---|---|
| `--desk` | `#050507` | Window desk |
| `--background` | `#09090B` | Shell chrome |
| `--panel` | `#100F13` | Content islands |
| `--raised` | `#1A181E` | Controls |
| `--line` | `#232128` | Borders |
| `--line-2` | `#37333D` | Emphasized borders |
| `--divider` | `#18161B` | Row separators |
| `--strong` | `#F7F4F7` | Selected and heading text |
| `--foreground` | `#C6C3CC` | Body text |
| `--muted` | `#8E8996` | Secondary text |
| `--info` | `#6EA8FF` | File type icon |
| `--success` | `#7BE0A0` | Available volume |
| `--warning` | `#FFC23D` | Warning |
| `--danger` | `#FF6B5E` | Error |
| `--hover` | `rgba(255,255,255,.04)` | Unselected hover |
| `--edge` | `rgba(255,255,255,.05)` | Island edge |

All above: Bolt colors/mod.rs. No separate Fileflux brand exists.

## Provisional brand
Reuse ReqFlux's existing cobalt for comparison, **not an approved Fileflux identity**. Source: /home/iperez/dev/personal/Rust/reqflux/crates/reqflux_ui/src/brand.rs:7–16.
| Token | Value | Usage |
|---|---|---|
| `--accent` | `#2B45B8` | Filled brand control |
| `--tint` | `#9DB1FF` | Focus, selection marker |
| `--selection` | `rgba(157,177,255,.12)` | Item selection; Bolt brand.rs |
| `--on-accent` | `#FFFFFF` | On brand fill |

Blue folder icons reuse Bolt info, not the brand accent. This is a Fileflux-specific visual proposal, not an inherited folder token.

## Typography
Production Bolt uses Archivo (UI), Archivo Expanded (uppercase labels), JetBrains Mono (data). Artboards use system-ui and ui-monospace only: required self-contained preview fallback, not a font change proposal.
| Token | Value | Usage |
|---|---|---|
| `--ui` | `system-ui, -apple-system, "Segoe UI", sans-serif` | Preview UI |
| `--mono` | `ui-monospace, "SFMono-Regular", Consolas, monospace` | Preview paths and metadata |
Body 13px/500; body-small and caption 12px/500; labels 11px/800 uppercase with .14em tracking. Source: Bolt primitives/text.rs:24–43,306–360.

## Geometry
| Token | Value | Usage |
|---|---|---|
| `--sidebar` | `290px` | Standard shell sidebar |
| `--titlebar` | `44px` | Titlebar |
| `--statusbar` | `38px` | Statusbar |
| `--control` | `30px` | Controls |
| `--tree-row` | `26px` | Sidebar rows |
| `--gap` | `8px` | Between islands |
| `--cut` | `14px` | Island top-left / bottom-right chamfer |
Source: Bolt tokens.rs. Spacing 4,6,8,12,16,24px; radius 0; border 1px; selection marker 2px; keyboard focus 1.5px tint. No decorative shadows.

## Component rules
- Chosen Fileflux shell (01): desk, equal 8px top and side gutters, no in-window title or brand, sidebar and document islands, 38px statusbar. Source: src/explorer/view.rs uses IslandMetrics::GAP. The inherited 44px titlebar token above remains a Bolt reference, not an extra Fileflux strip; alternative artboards retain their original chrome. No activity rail unless needed; Fileflux has one primary job.
- Rows remain square. Selection uses wash plus 2px tint edge. Hover and keyboard focus are distinct.
- Islands use panel fill and 14px opposing chamfers. Controls use 6px chamfers.
- Stroke icons: viewBox 24, width 2, currentColor, no external assets.
- Grid tiles draw a compact filled folder silhouette in info: a 16px tab over a 40×32px body composed natively from filled geometry, because the bundled Lucide folder glyph is stroke-only; it is a silhouette, not a square colored app-icon chip. Source: src/explorer/entries.rs `folder_silhouette`.
- File glyphs are the bundled Lucide `file-*` family chosen by extension alone (image, video, audio, archive, spreadsheet, code, text; plain file otherwise): no file contents, no MIME probes, dotfiles stay plain. Source: src/explorer/entries.rs `file_glyph`.
- Grid tile names are the primary text at 13px over two 18px lines, center-aligned and clamped; the full name shows as a native tooltip in the preview (GPUI has no string tooltip API for stateful rows, so the native tile adds none). Tile geometry: 104px wide, 96px tall — 12px top, 32px icon slot, 8px gap, two lines, 8px bottom; compact variant 88px tiles with a 28px slot. Source: src/explorer/entries.rs constants and `tile_height_matches_its_composition_and_slot`.
- Entries container padding is grid-only: 16px sides and 24px bottom in Grid; the details list is edge-to-edge full width with no ordinal gutter, rows keeping only their inner 12px cell padding. Source: src/explorer/entries.rs `render`.
- Path and search stay distinct. Search labels its current-folder scope; this preview filters synthetic entries only.
- Folder grids have no existing Bolt primitive. Tile geometry (104×96px normal / 88px compact) is a proposal composed from Bolt spacing.
- Variant 03 uses a proposed 220px compact sidebar rather than the canonical 290px; all other shell dimensions remain inherited.
- No real filesystem access, file writes, trash operations, or mount actions. Demo volume sizes are synthetic.
- Narrow preview: collapsible sidebar, wrapping toolbar, horizontally scrollable metadata list. Keyboard focus remains visible.

## Self-check
Artboards restrict colors to the tables above and fonts to the two preview families. Selection opacity follows brand.rs. Semantic colors retain their roles; no arbitrary decorative palette. Dark theme only for this first design brief.
