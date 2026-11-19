<!-- Title: Conventional Commit, e.g. `fix(trash): keep the original name on restore` -->

## What and why

<!-- One paragraph: what changes and why. Link the issue (Closes #123). -->

## Review first

<!-- Where to start reading, and what is intentionally out of scope. -->

## Checks

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `cargo test --locked`
- [ ] `cargo build --release --locked`
- [ ] Tests added or updated for the behavior change (a bug fix has a regression test)
- [ ] Docs and the `[Unreleased]` section of `CHANGELOG.md` updated for visible changes
- [ ] Commits follow Conventional Commits and are small and reviewable

## Risk notes for file operations

<!-- Delete this section if the change does not touch create, rename, copy, move, Trash, restore, delete, undo or devices. -->

- [ ] Del still moves to the Trash; permanent deletion stays behind a confirmation whose default is Cancel
- [ ] Nothing is overwritten without an explicit Replace (`RENAME_NOREPLACE` or the existing fallback)
- [ ] Copies are built under a temporary name and renamed into place; sources are removed only after verification
- [ ] Undo is still identity-checked (device and inode)
- [ ] Symlinks are never followed; no shell is involved
- [ ] Tested on a scratch directory, not real data. How:

## UI changes

<!-- Screenshots or a short description of how you checked it. GUI checks are not run in CI. -->
