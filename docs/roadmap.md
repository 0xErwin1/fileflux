# Roadmap

What Fileflux still needs to stand in for a desktop file manager such as PCManFM, plus one feature beyond it, the disk usage analyzer, whose first version has shipped. The list comes from an audit of the documentation and a keyword search of `src/` on 2026-11-20; nothing here was checked by running the application.

## Quick path

1. Start with [Priority 1](#priority-1-daily-use-gaps): the gaps felt in everyday use.
2. Resolve the [open questions](#open-questions) before planning the items that depend on them.
3. Read [Disk usage analyzer](#disk-usage-analyzer) for what the shipped analyzer still lacks.

## Where Fileflux already leads

Undo, the dual pane, type/date/size filters, live refresh of every tab, rubber-band selection, the built-in image viewer and the eject safety rules have no PCManFM equivalent. They are not on this list.

## Priority 1: daily-use gaps

| Item | What is missing | Today |
|---|---|---|
| Owner helper in the prebuilt release | The nightly tarball (`.github/workflows/nightly.yml`) packages only `bin/fileflux`: it must also ship `bin/fileflux-owner-helper` and `packaging/dev.fileflux.owner-helper.policy`. Distribution packages must install the helper next to `fileflux` and the policy into `share/polkit-1/actions` with the helper's installed path. Before the nightly ships it, `nix/binary.nix` must keep the source and nightly packages installable together: install the helper under `libexec/` per channel and give the policy file and action id a channel suffix, or both packages provide the same `bin/fileflux-owner-helper` and policy. An install outside the Nix store or a root-owned prefix (for example `cargo install`) uses a helper the user can replace: Fileflux could refuse a helper not owned by root or writable by others, at the cost of single-user Nix, whose store the user owns | The Nix source build installs the helper and the policy; without the helper Fileflux falls back to `pkexec chown --no-dereference --from=+<uid of the shown owner>`, which re-checks the file before and after but cannot stop a parent folder swapped for a link during the authentication dialog |

## Priority 2: expected file manager features

| Item | What is missing | Today |
|---|---|---|

## Priority 3: refinements

| Item | What is missing | Today |
|---|---|---|
| Tabs | Drag a tab to another window (the file explorer has one window; this needs more than one) | Tabs reorder within the window and move between the two panes |
| Translations | A translated interface | English only |

## Disk usage analyzer

The analyzer shipped: **Analyze Disk Usage** in the empty-space menu of a folder and in the menu of a selected folder opens a ranked list with a sunburst chart, drill-down, and cleanup through the existing Trash and delete paths (see [Disk usage](user-guide.md#disk-usage)). What remains:

| Item | What is missing |
|---|---|
| Free-space estimate | The status line labels the totals an estimate after a cleanup; it does not compare them with the volume's free space, which the sidebar meter shows |
| Very large trees | The whole tree is held in memory (folders in full, 200 files per folder). A multi-million-folder volume needs a bounded or on-disk form |
| Chart extras | No keyboard navigation of the chart, no selectable "Other" slice, no zoom of the rings beyond three levels |
| Network and gvfs mounts | They are walked like any folder; the details panel's size asks first on them, the analyzer does not |

## Out of scope unless decided otherwise

- Desktop management (icons and wallpaper on the root window), which PCManFM offers through `--desktop`.
- Formatting and partitioning drives.

## Open questions

The audit did not settle these:

- [ ] Is there a preferences dialog, or are settings edited only through the TOML files?
- [ ] Is there a way to open a folder with elevated privileges?
- [ ] What happens when opening an executable file or a `.desktop` launcher?
- [ ] Is Fileflux registered as a handler for `inode/directory`?
- [ ] Can favorites be renamed or reordered from the sidebar?
- [ ] Do the mouse back and forward buttons navigate in the explorer?
