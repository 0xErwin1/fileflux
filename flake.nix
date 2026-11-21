{
  description = "Fileflux - a file explorer in the Bolt family";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    let
      nightlyInfo = import ./nix/nightly-info.nix;

      # Systems that ship a prebuilt binary in the rolling nightly release.
      # Other systems can still use the source build.
      nightlySystems = builtins.attrNames nightlyInfo.artifacts;

      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      fileffluxVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

      # Builds the Fileflux packages from a package set. The flake's own
      # outputs pass the nixpkgs pinned in flake.lock; the overlay passes the
      # consumer's package set, so the prebuilt binary is patched against, and
      # the source build links with, the same glibc as the rest of the system,
      # including the graphics drivers it loads at runtime.
      mkPackages =
        pkgs:
        let
          lib = pkgs.lib;
          system = pkgs.stdenv.hostPlatform.system;

          # Native libraries GPUI needs to build and run.
          buildInputs = with pkgs; [
            openssl
            zstd
            zlib
            freetype
            fontconfig
            wayland
            libxkbcommon
            libx11
            libxcb
            libxcursor
            libxrandr
            libxi
            vulkan-loader
            alsa-lib
          ];

          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake
            rustPlatform.bindgenHook
            mold
          ];

          # GPUI loads these with dlopen at runtime. Host graphics drivers
          # remain the host platform's responsibility; the dev shell supplies
          # the application libraries below, not an additional driver path.
          runtimeLibraryPath = lib.makeLibraryPath (
            with pkgs;
            [
              wayland
              libxkbcommon
              vulkan-loader
              libx11
              libxcb
              fontconfig
            ]
          );

          # Only tracked files reach the build in a git checkout, but filter
          # the common build output anyway so a plain directory copy also
          # builds.
          fileffluxSrc = lib.cleanSourceWith {
            src = ./.;
            filter =
              path: _type:
              (builtins.match ".*\\.git" path) == null
              && !lib.hasInfix "/target/" path
              && (builtins.match ".*(/target)" path) == null
              && (builtins.match ".*(/\\.direnv)" path) == null
              # Root `nix build` symlinks: the final segment must be exactly
              # `result` or `result-*`, so `results.rs` and a `results/`
              # module survive.
              && (builtins.match ".*(/result)" path) == null
              && (builtins.match ".*(/result-[^/]*)" path) == null;
          };

          # Source build: compiles locally.
          fileffluxSource = pkgs.rustPlatform.buildRustPackage {
            pname = "fileflux";
            version = fileffluxVersion;
            src = fileffluxSrc;

            cargoLock = {
              lockFile = ./Cargo.lock;
              allowBuiltinFetchGit = true;
            };

            inherit buildInputs;
            nativeBuildInputs = nativeBuildInputs ++ [ pkgs.makeWrapper ];

            # Tests run in CI (.github/workflows/ci.yml), not here: the
            # build sandbox has no display for GUI checks.
            doCheck = false;

            postInstall = ''
              install -Dm644 ${./packaging/fileflux.desktop} $out/share/applications/fileflux.desktop
              install -Dm644 ${./assets/icon/fileflux.svg} $out/share/icons/hicolor/scalable/apps/fileflux.svg
              install -Dm644 ${./assets/icon/fileflux-16.png} $out/share/icons/hicolor/16x16/apps/fileflux.png
              install -Dm644 ${./assets/icon/fileflux-32.png} $out/share/icons/hicolor/32x32/apps/fileflux.png
              install -Dm644 ${./assets/icon/fileflux-48.png} $out/share/icons/hicolor/48x48/apps/fileflux.png
              install -Dm644 ${./assets/icon/fileflux-64.png} $out/share/icons/hicolor/64x64/apps/fileflux.png
              install -Dm644 ${./assets/icon/fileflux-128.png} $out/share/icons/hicolor/128x128/apps/fileflux.png
              install -Dm644 ${./assets/icon/fileflux-256.png} $out/share/icons/hicolor/256x256/apps/fileflux.png
            '';

            postFixup = lib.optionalString pkgs.stdenv.hostPlatform.isLinux ''
              wrapProgram $out/bin/fileflux \
                --prefix LD_LIBRARY_PATH : "${runtimeLibraryPath}" \
                --suffix LD_LIBRARY_PATH : "/run/opengl-driver/lib:/usr/lib/x86_64-linux-gnu:/usr/lib64:/usr/lib" \
                --suffix PATH : "${lib.makeBinPath [ pkgs.libarchive ]}"
            '';

            meta = with lib; {
              description = "A file explorer in the Bolt family";
              homepage = "https://github.com/0xErwin1/fileflux";
              license = licenses.asl20;
              platforms = platforms.linux;
              mainProgram = "fileflux";
            };
          };

          # Rolling nightly prebuilt, pinned on the `nightly` ref. On `main`
          # the hashes in nightly-info.nix are placeholders; always consume
          # this via `github:0xErwin1/fileflux/nightly#fileflux-nightly`.
          hasNightly = builtins.elem system nightlySystems;
          fileffluxNightly =
            if hasNightly then
              pkgs.callPackage ./nix/binary.nix { infoFile = ./nix/nightly-info.nix; }
            else
              null;

          # Fileflux has no stable release yet, so the default package is the
          # source build.
          fileffluxDefault = fileffluxSource;
        in
        {
          inherit
            buildInputs
            nativeBuildInputs
            runtimeLibraryPath
            fileffluxSource
            hasNightly
            fileffluxNightly
            fileffluxDefault
            ;
        };

      # Per-system outputs (packages, devShells, apps).
      perSystem = flake-utils.lib.eachSystem supportedSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };

          # The exact toolchain rust-toolchain.toml pins, not a global install.
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          inherit (mkPackages pkgs)
            buildInputs
            nativeBuildInputs
            runtimeLibraryPath
            fileffluxSource
            fileffluxNightly
            fileffluxDefault
            ;

          # Decided from `system` alone, not from `pkgs`, so listing the
          # outputs never imports nixpkgs for a system it no longer supports.
          hasNightly = builtins.elem system nightlySystems;
        in
        {
          devShells.default = pkgs.mkShell {
            inherit buildInputs;
            # libarchive provides bsdtar, which Compress/Extract run and the
            # archive tests exercise.
            nativeBuildInputs = nativeBuildInputs ++ [
              rustToolchain
              pkgs.libarchive
            ];
            LD_LIBRARY_PATH = runtimeLibraryPath;
            # Fontconfig is dlopened at runtime (see .cargo/config.toml); keep
            # it resolvable for direct binary runs outside cargo too.
            RUST_FONTCONFIG_DLOPEN = "1";
          };

          # Packages:
          #   .default           -> source build (no stable release yet)
          #   .fileflux          -> alias for .default
          #   .fileflux-source   -> explicit source build
          #   .fileflux-nightly  -> rolling nightly prebuilt (pin to nightly ref)
          packages = {
            default = fileffluxDefault;
            fileflux = fileffluxDefault;
            fileflux-source = fileffluxSource;
          }
          // nixpkgs.lib.optionalAttrs hasNightly { fileflux-nightly = fileffluxNightly; };

          apps = {
            default = flake-utils.lib.mkApp {
              drv = fileffluxDefault;
              exePath = "/bin/fileflux";
            };
            fileflux = flake-utils.lib.mkApp {
              drv = fileffluxDefault;
              exePath = "/bin/fileflux";
            };
          }
          // nixpkgs.lib.optionalAttrs hasNightly {
            fileflux-nightly = flake-utils.lib.mkApp {
              drv = fileffluxNightly;
              exePath = "/bin/fileflux-nightly";
            };
          };

          formatter = pkgs.nixfmt-rfc-style;
        }
      );
    in
    perSystem
    // {
      # Overlay for downstream consumers:
      #
      #   nixpkgs.overlays = [ inputs.fileflux.overlays.default ];
      #   home.packages = [ pkgs.fileflux-nightly ];
      #
      # `pkgs.fileflux`          -> built from source
      # `pkgs.fileflux-source`   -> built from source (explicit)
      # `pkgs.fileflux-nightly`  -> rolling nightly prebuilt (only on nightly systems)
      #
      # Every package is built from the consumer's own package set, not from
      # this flake's nixpkgs, so it uses the same glibc as the rest of the
      # system, including the graphics drivers it loads at runtime.
      #
      # Which attributes exist is decided from `prev` only: deciding it from
      # `final` would make the overlay's own attribute names depend on the
      # fixpoint it is part of, which is an infinite recursion.
      overlays.default =
        final: prev:
        let
          system = prev.stdenv.hostPlatform.system;
          hasSystem = perSystem.packages ? ${system};
          hasNightly = builtins.elem system nightlySystems;
          consumerPkgs = mkPackages final;
        in
        if hasSystem then
          {
            fileflux = consumerPkgs.fileffluxDefault;
            fileflux-source = consumerPkgs.fileffluxSource;
          }
          // nixpkgs.lib.optionalAttrs hasNightly { fileflux-nightly = consumerPkgs.fileffluxNightly; }
        else
          { };
    };
}
