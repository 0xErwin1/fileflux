# Roadmap

What Fileflux still needs to stand in for a desktop file manager such as PCManFM, plus one feature beyond it, the disk usage analyzer, whose first version has shipped. The list comes from an audit of the documentation and a keyword search of `src/` on 2026-11-20; nothing here was checked by running the application.

## Quick path

1. Start with [Priority 1](#priority-1-daily-use-gaps): the gaps felt in everyday use.
2. Read [Disk usage analyzer](#disk-usage-analyzer) for what the shipped analyzer still lacks.

## Where Fileflux already leads

Undo, the dual pane, the Run / Run in Terminal / Open prompt for programs, type/date/size filters, live refresh of every tab, rubber-band selection, the built-in image viewer and the eject safety rules have no PCManFM equivalent. They are not on this list.

## Priority 1: daily-use gaps

| Item | What is missing | Today |
|---|---|---|

## Priority 2: expected file manager features

| Item | What is missing | Today |
|---|---|---|

## Priority 3: refinements

| Item | What is missing | Today |
|---|---|---|
| Owner helper outside Nix | An install outside the Nix store or a root-owned prefix (for example `cargo install`) uses a helper the user can replace: Fileflux could refuse a helper not owned by root or writable by others, at the cost of single-user Nix, whose store the user owns | The helper is used wherever it is found beside the launcher (or under `libexec/<channel>/`) |
| Tabs | Drag a tab to another window (the file explorer has one window; this needs more than one) | Tabs reorder within the window and move between the two panes |
| Translations | A translated interface | English only |

## Disk usage analyzer

The analyzer shipped: **Analyze Disk Usage** in the empty-space menu of a folder and in the menu of a selected folder opens a ranked list with a sunburst chart, drill-down, and cleanup through the existing Trash and delete paths (see [Disk usage](user-guide.md#disk-usage)). What remains:

| Item | What is missing |
|---|---|
| Very large trees | The whole tree is held in memory (folders in full, 200 files per folder). A multi-million-folder volume needs a bounded or on-disk form |

## Out of scope unless decided otherwise

- Desktop management (icons and wallpaper on the root window), which PCManFM offers through `--desktop`.
- Formatting and partitioning drives.
- A preferences screen: settings stay in hand-editable TOML files (see [Settings](settings.md)).
