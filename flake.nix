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
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        lib = pkgs.lib;

        # The exact toolchain rust-toolchain.toml pins, not a global install.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

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
        # remain the host platform's responsibility; this shell supplies
        # the application libraries below, not an additional driver path.
        runtimeLibraryPath = lib.makeLibraryPath (
          with pkgs; [
            wayland
            libxkbcommon
            vulkan-loader
            libx11
            libxcb
            fontconfig
          ]
        );
      in
      {
        devShells.default = pkgs.mkShell {
          inherit buildInputs;
          nativeBuildInputs = nativeBuildInputs ++ [ rustToolchain ];
          LD_LIBRARY_PATH = runtimeLibraryPath;
          # Fontconfig is dlopened at runtime (see .cargo/config.toml); keep
          # it resolvable for direct binary runs outside cargo too.
          RUST_FONTCONFIG_DLOPEN = "1";
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    );
}
