# Fileflux

A fast GPUI file manager for Linux, written in Rust. Fileflux is part of the Bolt family, built on the same GPUI stack and design system as [DBFlux](https://github.com/0xErwin1/dbflux) and [ReqFlux](https://github.com/0xErwin1/reqflux). It browses, organizes and manages files, and it is careful with them: nothing is deleted or overwritten silently.

## Features

- Tabs, grid and list views, breadcrumbs and an editable path field, with every tab's folder kept live: [User guide](docs/user-guide.md).
- Sort and filter by type, date, size, name and hidden files: [Sort and filter](docs/user-guide.md#sort-and-filter).
- Create, rename, copy, cut, paste and drag and drop, with progress, conflict handling and Undo: [File operations](docs/user-guide.md#file-operations).
- Del moves to the freedesktop Trash; permanent delete always asks first: [Trash](docs/user-guide.md#trash).
- Favorites are your GTK bookmarks, shared with Nautilus and Thunar: [Favorites](docs/user-guide.md#favorites-and-gtk-bookmarks).
- Mount, unmount and eject drives, and browse phones (MTP) and network shares (gvfs): [Devices](docs/devices.md).
- Open with…, Open in terminal, and a built-in image viewer: [Open and terminal](docs/user-guide.md#open-open-with-and-terminal), [Image viewer](docs/user-guide.md#image-viewer).
- Preferences and open tabs are restored at the next launch: [Settings and files](docs/settings.md).

## Install

Fileflux is Linux-only and has no stable release yet.

**Prebuilt nightly, with Nix** (x86_64-linux; installs the binary as `fileflux-nightly`):

```sh
nix run github:0xErwin1/fileflux/nightly#fileflux-nightly
```

Use the `nightly` ref: the hashes of the prebuilt binary are only valid there, not on `main`. The flake also offers an overlay (`overlays.default`) that provides `pkgs.fileflux`, `pkgs.fileflux-source` and `pkgs.fileflux-nightly`.

**From source, with Nix:**

```sh
nix build github:0xErwin1/fileflux#fileflux
```

> **Known issue:** this build currently fails while vendoring the `gpui-component` git dependency (ReqFlux's source build fails the same way). Until that is fixed, use the nightly package or build with Cargo.

**From source, with Cargo** inside the Nix dev shell, which provides the pinned toolchain and native libraries:

```sh
git clone https://github.com/0xErwin1/fileflux
cd fileflux
nix develop --command cargo build --release --locked
./target/release/fileflux
```

The repository owner `0xErwin1` is an assumption carried by the flake and the workflows; update it if the repository lives elsewhere. See [Development](docs/development.md) for the direnv setup, checks and scratch-folder practice.

## Usage

```text
fileflux [PATH]    open a folder (default: the saved tabs, or your home folder)
fileflux IMAGE     open the image viewer on a picture and its folder
```

Every option and exit status is in the [command line reference](docs/user-guide.md#command-line). To make Fileflux your default image viewer, see [Making Fileflux your default image viewer](docs/user-guide.md#making-fileflux-your-default-image-viewer).

## Key shortcuts

| Key | Action |
|---|---|
| Ctrl+T / Ctrl+W | New / close tab |
| Ctrl+L | Edit the path |
| Ctrl+F | Search file names |
| Ctrl+H | Show or hide hidden files |
| Alt+← / Alt+→ / Alt+↑ | Back / forward / up |
| F2 | Rename |
| Del / Shift+Del | Move to Trash / delete permanently |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / cut / paste |
| Ctrl+Z | Undo |
| Ctrl+I | Toggle the details panel |

All of them, including the image viewer's: [Keyboard shortcuts](docs/keyboard-shortcuts.md).

## Documentation

The [documentation index](docs/README.md) links the [user guide](docs/user-guide.md), [keyboard shortcuts](docs/keyboard-shortcuts.md), [settings and files](docs/settings.md), [devices](docs/devices.md), [architecture](docs/architecture.md) and [development](docs/development.md). Changes are listed in the [changelog](CHANGELOG.md).

## Contributing

Fileflux moves and deletes people's files, so correctness and safety come first. Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup, checks and commit conventions.

## License

Apache-2.0. See [LICENSE](LICENSE).
